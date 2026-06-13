use crate::request::Request;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;

#[derive(Debug)]
pub enum CgiError {
    ForkFailed,
    PipeFailed,
    Timeout,
    Io(String),
}

impl std::fmt::Display for CgiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CgiError::ForkFailed => write!(f, "fork() failed"),
            CgiError::PipeFailed => write!(f, "pipe() failed"),
            CgiError::Timeout => write!(f, "CGI script timed out"),
            CgiError::Io(s) => write!(f, "CGI I/O error: {}", s),
        }
    }
}

pub struct CgiResponse {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub status: u16,
}

pub fn run_cgi(script_path: &str, req: &Request, cgi_binary: &str) -> Result<CgiResponse, CgiError> {
    // Create pipes: parent reads from child stdout, parent writes to child stdin
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

            // Get the directory of the script for working directory
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

        // Write request body to child stdin
        if !req.body.is_empty() {
            let mut stdin_write = std::fs::File::from_raw_fd(stdin_pipe[1]);
            stdin_write
                .write_all(&req.body)
                .map_err(|e| CgiError::Io(e.to_string()))?;
            // stdin_write drops here, closing the fd (signals EOF to CGI)
        } else {
            libc::close(stdin_pipe[1]);
        }

        // Read child stdout
        let mut stdout_read = std::fs::File::from_raw_fd(stdout_pipe[0]);
        let mut raw_output = Vec::new();
        stdout_read
            .read_to_end(&mut raw_output)
            .map_err(|e| CgiError::Io(e.to_string()))?;

        // Wait for child
        let mut status = 0i32;
        libc::waitpid(pid, &mut status, 0);

        parse_cgi_output(raw_output)
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

fn parse_cgi_output(raw: Vec<u8>) -> Result<CgiResponse, CgiError> {
    // CGI output: headers, blank line, then body
    let separator = b"\r\n\r\n";
    let split_pos = raw
        .windows(4)
        .position(|w| w == separator)
        .or_else(|| {
            // Also accept \n\n
            raw.windows(2).position(|w| w == b"\n\n")
        });

    let (header_bytes, body) = if let Some(pos) = split_pos {
        let sep_len = if raw[pos] == b'\r' { 4 } else { 2 };
        (&raw[..pos], raw[pos + sep_len..].to_vec())
    } else {
        return Ok(CgiResponse {
            headers: Vec::new(),
            body: raw,
            status: 200,
        });
    };

    let header_str = std::str::from_utf8(header_bytes).map_err(|_| CgiError::Io("Non-UTF8 headers".to_string()))?;
    let mut headers = Vec::new();
    let mut status = 200u16;

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

    Ok(CgiResponse { headers, body, status })
}
