use crate::request::Request;
use std::os::unix::io::RawFd;

#[derive(Debug)]
pub enum CgiError {
    ForkFailed,
    PipeFailed,
}

impl std::fmt::Display for CgiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CgiError::ForkFailed => write!(f, "fork() failed"),
            CgiError::PipeFailed => write!(f, "pipe() failed"),
        }
    }
}

pub struct CgiResponse {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub status: u16,
}

/// A CGI child process that has been spawned but not yet finished. Its stdin
/// pipe (if there's a body to send) and stdout pipe are meant to be driven
/// from the caller's epoll loop — no blocking I/O happens here.
pub struct CgiProcess {
    pub pid: libc::pid_t,
    pub stdin_fd: Option<RawFd>,
    pub stdin_data: Vec<u8>,
    pub stdin_offset: usize,
    pub stdout_fd: RawFd,
}

pub fn spawn_cgi(script_path: &str, req: &Request, cgi_binary: &str) -> Result<CgiProcess, CgiError> {
    let mut stdin_pipe = [0i32; 2];
    let mut stdout_pipe = [0i32; 2];

    unsafe {
        if libc::pipe(stdin_pipe.as_mut_ptr()) != 0 {
            return Err(CgiError::PipeFailed);
        }
        if libc::pipe(stdout_pipe.as_mut_ptr()) != 0 {
            libc::close(stdin_pipe[0]);
            libc::close(stdin_pipe[1]);
            return Err(CgiError::PipeFailed);
        }

        let pid = libc::fork();
        if pid < 0 {
            libc::close(stdin_pipe[0]);
            libc::close(stdin_pipe[1]);
            libc::close(stdout_pipe[0]);
            libc::close(stdout_pipe[1]);
            return Err(CgiError::ForkFailed);
        }

        if pid == 0 {
            // Child process
            libc::close(stdin_pipe[1]);
            libc::close(stdout_pipe[0]);
            libc::dup2(stdin_pipe[0], libc::STDIN_FILENO);
            libc::dup2(stdout_pipe[1], libc::STDOUT_FILENO);
            libc::close(stdin_pipe[0]);
            libc::close(stdout_pipe[1]);

            setup_cgi_env(script_path, req);

            let script_dir = std::path::Path::new(script_path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or(".");
            let dir_cstr = std::ffi::CString::new(script_dir).unwrap();
            libc::chdir(dir_cstr.as_ptr());

            let bin_cstr = std::ffi::CString::new(cgi_binary).unwrap();
            let script_cstr = std::ffi::CString::new(script_path).unwrap();
            let args: Vec<*const libc::c_char> = vec![
                bin_cstr.as_ptr(),
                script_cstr.as_ptr(),
                std::ptr::null(),
            ];
            libc::execv(bin_cstr.as_ptr(), args.as_ptr());
            libc::exit(1);
        }

        // Parent process
        libc::close(stdin_pipe[0]);
        libc::close(stdout_pipe[1]);

        let _ = crate::event_loop::set_nonblocking(stdout_pipe[0]);

        let stdin_fd = if !req.body.is_empty() {
            let _ = crate::event_loop::set_nonblocking(stdin_pipe[1]);
            Some(stdin_pipe[1])
        } else {
            libc::close(stdin_pipe[1]);
            None
        };

        Ok(CgiProcess {
            pid,
            stdin_fd,
            stdin_data: req.body.clone(),
            stdin_offset: 0,
            stdout_fd: stdout_pipe[0],
        })
    }
}

unsafe fn setup_cgi_env(script_path: &str, req: &Request) {
    let method = req.method.as_str();
    let content_length = req.body.len().to_string();
    let content_type = req
        .headers
        .get("content-type")
        .map(|s| s.as_str())
        .unwrap_or("");

    unsafe {
        set_env("REQUEST_METHOD", method);
        set_env("PATH_INFO", script_path);
        set_env("SCRIPT_FILENAME", script_path);
        set_env("QUERY_STRING", &req.query_string);
        set_env("CONTENT_LENGTH", &content_length);
        set_env("CONTENT_TYPE", content_type);
        set_env("SERVER_PROTOCOL", "HTTP/1.1");
        set_env("GATEWAY_INTERFACE", "CGI/1.1");
        set_env("SERVER_SOFTWARE", "localhost/1.0");

        if let Some(host) = req.headers.get("host") {
            set_env("HTTP_HOST", host);
        }
        if let Some(cookie) = req.headers.get("cookie") {
            set_env("HTTP_COOKIE", cookie);
        }
    }
}

unsafe fn set_env(key: &str, value: &str) {
    let k = std::ffi::CString::new(key).unwrap();
    let v = std::ffi::CString::new(value).unwrap();
    unsafe { libc::setenv(k.as_ptr(), v.as_ptr(), 1) };
}

/// Reap a finished/killed child without blocking. Safe to call repeatedly;
/// returns true once the child has actually been reaped.
pub fn try_reap(pid: libc::pid_t) -> bool {
    let mut status = 0i32;
    let ret = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    ret == pid
}

pub fn kill_and_reap(pid: libc::pid_t) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status = 0i32;
        libc::waitpid(pid, &mut status, 0);
    }
}

pub fn parse_cgi_output(raw: Vec<u8>) -> CgiResponse {
    let separator = b"\r\n\r\n";
    let split_pos = raw
        .windows(4)
        .position(|w| w == separator)
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n"));

    let (header_bytes, body) = if let Some(pos) = split_pos {
        let sep_len = if raw[pos] == b'\r' { 4 } else { 2 };
        (&raw[..pos], raw[pos + sep_len..].to_vec())
    } else {
        return CgiResponse {
            headers: Vec::new(),
            body: raw,
            status: 200,
        };
    };

    let mut headers = Vec::new();
    let mut status = 200u16;

    if let Ok(header_str) = std::str::from_utf8(header_bytes) {
        for line in header_str.lines() {
            if let Some(colon) = line.find(':') {
                let key = line[..colon].trim().to_string();
                let value = line[colon + 1..].trim().to_string();
                if key.to_lowercase() == "status" {
                    if let Ok(code) = value.splitn(2, ' ').next().unwrap_or("200").parse::<u16>() {
                        status = code;
                    }
                } else {
                    headers.push((key, value));
                }
            }
        }
    }

    CgiResponse { headers, body, status }
}
