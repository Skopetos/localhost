use crate::config::Config;
use crate::event_loop::{self, EventLoop, FdKind, CGI_TIMEOUT_SECS, CLIENT_TIMEOUT_SECS, MAX_EVENTS};
use crate::handler::{self, HandleOutcome};
use crate::request::{ParseError, RequestParser};
use crate::response::Response;
use crate::session::SessionStore;
use libc::{epoll_event, sockaddr_in, AF_INET, IPPROTO_TCP, SOCK_STREAM, SOL_SOCKET, SO_REUSEADDR};
use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Client {
    parser: RequestParser,
    write_buf: Vec<u8>,
    write_offset: usize,
    last_active: u64,
    port: u16,
    close_after_write: bool,
}

impl Client {
    fn new(max_body: usize, port: u16) -> Self {
        Client {
            parser: RequestParser::new(max_body),
            write_buf: Vec::new(),
            write_offset: 0,
            last_active: now_secs(),
            port,
            close_after_write: false,
        }
    }
}

/// State for a CGI child whose stdin/stdout pipes are registered in the
/// epoll set and driven a chunk at a time, instead of blocking the whole
/// server while the script runs.
struct CgiState {
    pid: libc::pid_t,
    stdin_fd: Option<RawFd>,
    stdin_data: Vec<u8>,
    stdin_offset: usize,
    stdout_fd: RawFd,
    output: Vec<u8>,
    deadline: Instant,
    keep_alive: bool,
}

pub fn run(config: Config) -> Result<(), String> {
    let mut el = EventLoop::new()?;
    let mut clients: HashMap<RawFd, Client> = HashMap::new();
    let mut cgi_procs: HashMap<RawFd, CgiState> = HashMap::new();
    let mut reap_queue: Vec<libc::pid_t> = Vec::new();
    let mut sessions = SessionStore::new();

    // Bind one listening socket per (host, port) pair across all server blocks.
    // A bind failure on one listener is logged and skipped rather than aborting
    // the whole process, so other correctly configured servers keep running.
    let mut bound: std::collections::HashSet<(String, u16)> = std::collections::HashSet::new();
    let mut bound_any = false;
    for server in &config.servers {
        for &port in &server.ports {
            let key = (server.host.clone(), port);
            if bound.contains(&key) {
                continue;
            }
            match create_listener(&server.host, port) {
                Ok(fd) => {
                    if let Err(e) = el.add_listener(fd, port) {
                        eprintln!("epoll: failed to register listener {}:{}: {}", server.host, port, e);
                        unsafe { libc::close(fd) };
                        continue;
                    }
                    bound.insert(key);
                    bound_any = true;
                    println!("Listening on {}:{}", server.host, port);
                }
                Err(e) => {
                    eprintln!(
                        "Warning: could not start listener {}:{} ({}) — skipping it, other servers continue",
                        server.host, port, e
                    );
                }
            }
        }
    }

    if !bound_any {
        return Err("No listening sockets could be bound; check the configuration".to_string());
    }

    let mut events: [epoll_event; MAX_EVENTS] = unsafe { std::mem::zeroed() };
    let mut tick: u64 = 0;

    loop {
        let n = el.wait(&mut events);
        if n < 0 {
            let err = event_loop::errno();
            if err == libc::EINTR {
                continue;
            }
            return Err(format!("epoll_wait error: errno {}", err));
        }

        for i in 0..n as usize {
            let fd = events[i].u64 as RawFd;
            let ev_flags = events[i].events;
            let kind = match el.fd_map.get(&fd).copied() {
                Some(k) => k,
                None => continue,
            };

            match kind {
                FdKind::Listener(port) => {
                    accept_clients(fd, port, &mut el, &mut clients, &config);
                }
                FdKind::Client => {
                    if ev_flags & libc::EPOLLIN as u32 != 0 {
                        read_client(fd, &mut el, &mut clients, &config, &mut sessions, &mut cgi_procs);
                    }
                    if ev_flags & libc::EPOLLOUT as u32 != 0 && clients.contains_key(&fd) {
                        write_client(fd, &mut el, &mut clients);
                    }
                    if ev_flags & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 && clients.contains_key(&fd)
                    {
                        el.remove(fd);
                        clients.remove(&fd);
                    }
                }
                FdKind::CgiStdout(client_fd) => {
                    if ev_flags & (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0 {
                        handle_cgi_stdout(fd, client_fd, &mut el, &mut clients, &mut cgi_procs, &mut reap_queue);
                    }
                }
                FdKind::CgiStdin(client_fd) => {
                    if ev_flags & libc::EPOLLOUT as u32 != 0 {
                        handle_cgi_stdin(fd, client_fd, &mut el, &mut cgi_procs);
                    }
                    if ev_flags & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
                        if let Some(state) = cgi_procs.get_mut(&client_fd) {
                            if let Some(sin) = state.stdin_fd.take() {
                                el.remove(sin);
                            }
                        }
                    }
                }
            }
        }

        // Periodically purge timed-out connections, expired sessions, hung
        // CGI scripts, and reap any child we couldn't reap immediately.
        tick += 1;
        if tick % 30 == 0 {
            timeout_clients(&mut el, &mut clients);
            sessions.purge_expired();
        }
        reap_queue.retain(|&pid| !crate::cgi::try_reap(pid));
        timeout_cgi(&mut el, &mut clients, &mut cgi_procs, &mut reap_queue);
    }
}

fn accept_clients(
    listener_fd: RawFd,
    port: u16,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
    config: &Config,
) {
    // One accept() per ready event; if more connections are queued, the
    // level-triggered listener fd will simply be reported again next round.
    let client_fd = unsafe { libc::accept(listener_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
    if client_fd < 0 {
        let err = event_loop::errno();
        if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
            return;
        }
        eprintln!("accept() error: errno {}", err);
        return;
    }

    if let Err(e) = event_loop::set_nonblocking(client_fd) {
        eprintln!("set_nonblocking failed: {}", e);
        unsafe { libc::close(client_fd) };
        return;
    }

    let max_body = config
        .servers
        .iter()
        .find(|s| s.ports.contains(&port))
        .map(|s| s.client_max_body_size)
        .unwrap_or(1024 * 1024);

    if let Err(e) = el.add_client(client_fd) {
        eprintln!("add_client failed: {}", e);
        unsafe { libc::close(client_fd) };
        return;
    }

    clients.insert(client_fd, Client::new(max_body, port));
}

fn read_client(
    fd: RawFd,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
    config: &Config,
    sessions: &mut SessionStore,
    cgi_procs: &mut HashMap<RawFd, CgiState>,
) {
    let mut buf = [0u8; 16384];
    // Exactly one read() per ready event: with level-triggered epoll, any
    // data left unread is reported again on the next epoll_wait() round.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        let err = event_loop::errno();
        if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
            return;
        }
        el.remove(fd);
        clients.remove(&fd);
        return;
    }
    if n == 0 {
        el.remove(fd);
        clients.remove(&fd);
        return;
    }

    let port;
    let parsed;
    {
        let client = match clients.get_mut(&fd) {
            Some(c) => c,
            None => return,
        };
        client.last_active = now_secs();
        client.parser.feed(&buf[..n as usize]);
        port = client.port;
        parsed = client.parser.try_parse();
    }

    match parsed {
        Ok(Some(req)) => {
            let keep_alive = req
                .headers
                .get("connection")
                .map(|v| v.to_lowercase() != "close")
                .unwrap_or(req.version == "HTTP/1.1");

            let outcome = handler::handle(&req, &config.servers, port, sessions);

            if let Some(client) = clients.get_mut(&fd) {
                client.parser.reset();
            } else {
                return;
            }

            match outcome {
                HandleOutcome::Response(resp) => {
                    finish_response(fd, resp, keep_alive, el, clients);
                }
                HandleOutcome::Cgi(proc) => {
                    start_cgi(fd, proc, keep_alive, el, clients, cgi_procs);
                }
            }
        }
        Ok(None) => {
            // Headers/body not complete yet; wait for more data.
        }
        Err(ParseError::TooLarge) => {
            send_error_and_close(fd, 413, el, clients);
        }
        Err(ParseError::Invalid(_)) => {
            send_error_and_close(fd, 400, el, clients);
        }
    }
}

fn finish_response(
    fd: RawFd,
    resp: Response,
    keep_alive: bool,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
) {
    let client = match clients.get_mut(&fd) {
        Some(c) => c,
        None => return,
    };

    let resp = resp.with_header("Connection", if keep_alive { "keep-alive" } else { "close" });
    let bytes = resp.serialize();

    client.write_buf = bytes;
    client.write_offset = 0;
    client.close_after_write = !keep_alive;

    if let Err(e) = el.set_writable(fd) {
        eprintln!("set_writable: {}", e);
        el.remove(fd);
        clients.remove(&fd);
    }
}

fn start_cgi(
    fd: RawFd,
    proc: crate::cgi::CgiProcess,
    keep_alive: bool,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
    cgi_procs: &mut HashMap<RawFd, CgiState>,
) {
    if let Err(e) = el.add_cgi_stdout(proc.stdout_fd, fd) {
        eprintln!("add_cgi_stdout failed: {}", e);
        unsafe { libc::close(proc.stdout_fd) };
        if let Some(sin) = proc.stdin_fd {
            unsafe { libc::close(sin) };
        }
        crate::cgi::kill_and_reap(proc.pid);
        send_error_and_close(fd, 500, el, clients);
        return;
    }

    if let Some(stdin_fd) = proc.stdin_fd {
        if let Err(e) = el.add_cgi_stdin(stdin_fd, fd) {
            eprintln!("add_cgi_stdin failed: {}", e);
            unsafe { libc::close(stdin_fd) };
        }
    }

    // Stop reading further request data from this client while its CGI
    // response is in flight; HUP/ERR still surface since the fd stays registered.
    let _ = el.set_idle(fd);

    cgi_procs.insert(
        fd,
        CgiState {
            pid: proc.pid,
            stdin_fd: proc.stdin_fd,
            stdin_data: proc.stdin_data,
            stdin_offset: proc.stdin_offset,
            stdout_fd: proc.stdout_fd,
            output: Vec::new(),
            deadline: Instant::now() + std::time::Duration::from_secs(CGI_TIMEOUT_SECS),
            keep_alive,
        },
    );
}

fn handle_cgi_stdin(
    pipe_fd: RawFd,
    client_fd: RawFd,
    el: &mut EventLoop,
    cgi_procs: &mut HashMap<RawFd, CgiState>,
) {
    let state = match cgi_procs.get_mut(&client_fd) {
        Some(s) => s,
        None => {
            el.remove(pipe_fd);
            return;
        }
    };

    let remaining = &state.stdin_data[state.stdin_offset..];
    if remaining.is_empty() {
        state.stdin_fd = None;
        el.remove(pipe_fd);
        return;
    }

    let n = unsafe { libc::write(pipe_fd, remaining.as_ptr() as *const libc::c_void, remaining.len()) };
    if n < 0 {
        let err = event_loop::errno();
        if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
            return;
        }
        // Child likely closed its stdin early (e.g. it doesn't read the body);
        // stop writing but keep waiting for its stdout.
        state.stdin_fd = None;
        el.remove(pipe_fd);
        return;
    }

    state.stdin_offset += n as usize;
    if state.stdin_offset >= state.stdin_data.len() {
        state.stdin_fd = None;
        el.remove(pipe_fd);
    }
}

fn handle_cgi_stdout(
    pipe_fd: RawFd,
    client_fd: RawFd,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
    cgi_procs: &mut HashMap<RawFd, CgiState>,
    reap_queue: &mut Vec<libc::pid_t>,
) {
    let mut buf = [0u8; 4096];
    let n = unsafe { libc::read(pipe_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };

    if n < 0 {
        let err = event_loop::errno();
        if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
            return;
        }
        finalize_cgi(client_fd, el, clients, cgi_procs, reap_queue);
        return;
    }

    if n == 0 {
        finalize_cgi(client_fd, el, clients, cgi_procs, reap_queue);
        return;
    }

    if let Some(state) = cgi_procs.get_mut(&client_fd) {
        state.output.extend_from_slice(&buf[..n as usize]);
    }
}

fn finalize_cgi(
    client_fd: RawFd,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
    cgi_procs: &mut HashMap<RawFd, CgiState>,
    reap_queue: &mut Vec<libc::pid_t>,
) {
    let state = match cgi_procs.remove(&client_fd) {
        Some(s) => s,
        None => return,
    };

    el.remove(state.stdout_fd);
    if let Some(sin) = state.stdin_fd {
        el.remove(sin);
    }

    if !crate::cgi::try_reap(state.pid) {
        reap_queue.push(state.pid);
    }

    let cgi_resp = crate::cgi::parse_cgi_output(state.output);
    let resp = handler::cgi_response_to_http(cgi_resp);
    finish_response(client_fd, resp, state.keep_alive, el, clients);
}

fn timeout_cgi(
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
    cgi_procs: &mut HashMap<RawFd, CgiState>,
    reap_queue: &mut Vec<libc::pid_t>,
) {
    let now = Instant::now();
    let timed_out: Vec<RawFd> = cgi_procs
        .iter()
        .filter(|(_, s)| now > s.deadline)
        .map(|(&fd, _)| fd)
        .collect();

    for client_fd in timed_out {
        if let Some(state) = cgi_procs.remove(&client_fd) {
            crate::cgi::kill_and_reap(state.pid);
            el.remove(state.stdout_fd);
            if let Some(sin) = state.stdin_fd {
                el.remove(sin);
            }
            let _ = reap_queue; // already reaped synchronously by kill_and_reap
            let resp = Response::error(504, None);
            finish_response(client_fd, resp, false, el, clients);
        }
    }
}

fn write_client(fd: RawFd, el: &mut EventLoop, clients: &mut HashMap<RawFd, Client>) {
    let client = match clients.get_mut(&fd) {
        Some(c) => c,
        None => return,
    };

    if client.write_offset >= client.write_buf.len() {
        finish_write(fd, el, clients);
        return;
    }

    // Exactly one write() per ready event, mirroring the read side: if the
    // socket can take more, level-triggered epoll reports EPOLLOUT again.
    let remaining = &client.write_buf[client.write_offset..];
    let n = unsafe { libc::write(fd, remaining.as_ptr() as *const libc::c_void, remaining.len()) };
    if n < 0 {
        let err = event_loop::errno();
        if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
            return; // will be called again when writable
        }
        el.remove(fd);
        clients.remove(&fd);
        return;
    }
    client.write_offset += n as usize;
    client.last_active = now_secs();

    if client.write_offset >= client.write_buf.len() {
        finish_write(fd, el, clients);
    }
}

fn send_error_and_close(
    fd: RawFd,
    code: u16,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
) {
    let resp = Response::error(code, None);
    let bytes = resp.serialize();
    unsafe {
        libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
    el.remove(fd);
    clients.remove(&fd);
}

fn timeout_clients(el: &mut EventLoop, clients: &mut HashMap<RawFd, Client>) {
    let now = now_secs();
    let timed_out: Vec<RawFd> = clients
        .iter()
        .filter(|(_, c)| now - c.last_active > CLIENT_TIMEOUT_SECS)
        .map(|(&fd, _)| fd)
        .collect();

    for fd in timed_out {
        let resp = Response::new(408).with_body(
            crate::response::default_error_page(408).into_bytes(),
            "text/html; charset=utf-8",
        );
        let bytes = resp.serialize();
        unsafe {
            libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
        }
        el.remove(fd);
        clients.remove(&fd);
    }
}

fn create_listener(host: &str, port: u16) -> Result<RawFd, String> {
    let fd = unsafe { libc::socket(AF_INET, SOCK_STREAM, IPPROTO_TCP) };
    if fd < 0 {
        return Err(format!("socket() failed: errno {}", event_loop::errno()));
    }

    // SO_REUSEADDR
    let opt: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            SOL_SOCKET,
            SO_REUSEADDR,
            &opt as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    event_loop::set_nonblocking(fd)?;

    let addr_bytes: u32 = parse_ipv4(host)?;
    let addr = sockaddr_in {
        sin_family: AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: addr_bytes },
        sin_zero: [0; 8],
    };

    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<sockaddr_in>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        unsafe { libc::close(fd) };
        return Err(format!("bind() failed on {}:{}: errno {}", host, port, event_loop::errno()));
    }

    let ret = unsafe { libc::listen(fd, 128) };
    if ret < 0 {
        unsafe { libc::close(fd) };
        return Err(format!("listen() failed: errno {}", event_loop::errno()));
    }

    Ok(fd)
}

fn parse_ipv4(host: &str) -> Result<u32, String> {
    if host == "0.0.0.0" {
        return Ok(libc::INADDR_ANY);
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return Err(format!("Invalid host address: {}", host));
    }
    let mut addr: u32 = 0;
    for (i, part) in parts.iter().enumerate() {
        let byte: u8 = part
            .parse()
            .map_err(|_| format!("Invalid IP octet: {}", part))?;
        addr |= (byte as u32) << (24 - i * 8);
    }
    Ok(addr.to_be())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
