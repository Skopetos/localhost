use crate::config::Config;
use crate::event_loop::{self, EventLoop, FdKind, CLIENT_TIMEOUT_SECS, MAX_EVENTS};
use crate::handler;
use crate::request::{ParseError, RequestParser};
use crate::response::Response;
use crate::session::SessionStore;
use libc::{epoll_event, sockaddr_in, AF_INET, IPPROTO_TCP, SOCK_STREAM, SOL_SOCKET, SO_REUSEADDR};
use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::time::{SystemTime, UNIX_EPOCH};

struct Client {
    parser: RequestParser,
    write_buf: Vec<u8>,
    write_offset: usize,
    last_active: u64,
    port: u16,
}

impl Client {
    fn new(max_body: usize, port: u16) -> Self {
        Client {
            parser: RequestParser::new(max_body),
            write_buf: Vec::new(),
            write_offset: 0,
            last_active: now_secs(),
            port,
        }
    }
}

pub fn run(config: Config) -> Result<(), String> {
    let mut el = EventLoop::new()?;
    let mut clients: HashMap<RawFd, Client> = HashMap::new();
    let mut sessions = SessionStore::new();

    // Bind one listening socket per (host, port) pair across all server blocks
    let mut bound_ports = std::collections::HashSet::new();
    for server in &config.servers {
        for &port in &server.ports {
            if bound_ports.contains(&port) {
                continue;
            }
            let fd = create_listener(&server.host, port)?;
            el.add_listener(fd, port)?;
            bound_ports.insert(port);
            println!("Listening on {}:{}", server.host, port);
        }
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
            let kind = match el.fd_map.get(&fd).copied() {
                Some(k) => k,
                None => continue,
            };

            match kind {
                FdKind::Listener(port) => {
                    accept_clients(fd, port, &mut el, &mut clients, &config);
                }
                FdKind::Client => {
                    let ev_flags = events[i].events;
                    if ev_flags & libc::EPOLLIN as u32 != 0 {
                        read_client(fd, &mut el, &mut clients, &config, &mut sessions);
                    }
                    if ev_flags & libc::EPOLLOUT as u32 != 0 {
                        write_client(fd, &mut el, &mut clients);
                    }
                    if ev_flags & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
                        el.remove(fd);
                        clients.remove(&fd);
                    }
                }
            }
        }

        // Periodically purge timed-out connections and expired sessions
        tick += 1;
        if tick % 30 == 0 {
            timeout_clients(&mut el, &mut clients);
            sessions.purge_expired();
        }
    }
}

fn accept_clients(
    listener_fd: RawFd,
    port: u16,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
    config: &Config,
) {
    loop {
        let client_fd = unsafe {
            libc::accept(listener_fd, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if client_fd < 0 {
            let err = event_loop::errno();
            if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
                break;
            }
            eprintln!("accept() error: errno {}", err);
            break;
        }

        if let Err(e) = event_loop::set_nonblocking(client_fd) {
            eprintln!("set_nonblocking failed: {}", e);
            unsafe { libc::close(client_fd) };
            continue;
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
            continue;
        }

        clients.insert(client_fd, Client::new(max_body, port));
    }
}

fn read_client(
    fd: RawFd,
    el: &mut EventLoop,
    clients: &mut HashMap<RawFd, Client>,
    config: &Config,
    sessions: &mut SessionStore,
) {
    let mut buf = [0u8; 16384];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let err = event_loop::errno();
            if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
                break;
            }
            el.remove(fd);
            clients.remove(&fd);
            return;
        }
        if n == 0 {
            // Connection closed
            el.remove(fd);
            clients.remove(&fd);
            return;
        }

        let client = match clients.get_mut(&fd) {
            Some(c) => c,
            None => return,
        };
        client.last_active = now_secs();
        client.parser.feed(&buf[..n as usize]);

        let port = client.port;

        match client.parser.try_parse() {
            Ok(Some(req)) => {
                let resp = handler::handle(&req, &config.servers, port, sessions);
                let keep_alive = req
                    .headers
                    .get("connection")
                    .map(|v| v.to_lowercase() != "close")
                    .unwrap_or(req.version == "HTTP/1.1");

                let mut bytes = resp.serialize();
                if keep_alive {
                    // Ensure Connection header is set
                    let conn_header = b"Connection: keep-alive\r\n";
                    // Insert before the blank line separating headers/body
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        bytes.splice(pos..pos, conn_header.iter().cloned());
                    }
                }

                client.write_buf = bytes;
                client.write_offset = 0;
                client.parser.reset();

                // Switch to write mode
                if let Err(e) = el.set_writable(fd) {
                    eprintln!("set_writable: {}", e);
                    el.remove(fd);
                    clients.remove(&fd);
                }
                return;
            }
            Ok(None) => {
                // Need more data
            }
            Err(ParseError::TooLarge) => {
                send_error_and_close(fd, 413, el, clients);
                return;
            }
            Err(ParseError::Invalid(_)) => {
                send_error_and_close(fd, 400, el, clients);
                return;
            }
        }
    }
}

fn write_client(fd: RawFd, el: &mut EventLoop, clients: &mut HashMap<RawFd, Client>) {
    let client = match clients.get_mut(&fd) {
        Some(c) => c,
        None => return,
    };

    while client.write_offset < client.write_buf.len() {
        let remaining = &client.write_buf[client.write_offset..];
        let n = unsafe {
            libc::write(fd, remaining.as_ptr() as *const libc::c_void, remaining.len())
        };
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
    }

    // All written — go back to reading
    client.write_buf.clear();
    client.write_offset = 0;
    if let Err(e) = el.set_readable(fd) {
        eprintln!("set_readable: {}", e);
        el.remove(fd);
        clients.remove(&fd);
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
