use libc::{
    epoll_create1, epoll_ctl, epoll_event, epoll_wait, EPOLLIN, EPOLLOUT, EPOLL_CTL_ADD,
    EPOLL_CTL_DEL, EPOLL_CTL_MOD,
};
use std::collections::HashMap;
use std::os::unix::io::RawFd;

pub const MAX_EVENTS: usize = 256;
pub const TIMEOUT_MS: i32 = 1000; // epoll_wait timeout in ms
pub const CLIENT_TIMEOUT_SECS: u64 = 30;
pub const CGI_TIMEOUT_SECS: u64 = 10;

// Level-triggered on purpose: it lets the dispatch loop do exactly one
// read()/write() per ready fd per epoll_wait() round and rely on epoll_wait()
// to report the fd again next round if more data remains, instead of having
// to drain in an inner loop (which edge-triggered mode would require).
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum FdKind {
    Listener(u16), // listening socket, associated port
    Client,
    CgiStdout(RawFd), // associated client fd
    CgiStdin(RawFd),  // associated client fd
}

pub struct EventLoop {
    pub epfd: RawFd,
    pub fd_map: HashMap<RawFd, FdKind>,
}

impl EventLoop {
    pub fn new() -> Result<Self, String> {
        let epfd = unsafe { epoll_create1(0) };
        if epfd < 0 {
            return Err(format!("epoll_create1 failed: errno {}", errno()));
        }
        Ok(EventLoop {
            epfd,
            fd_map: HashMap::new(),
        })
    }

    pub fn add_listener(&mut self, fd: RawFd, port: u16) -> Result<(), String> {
        self.add_fd(fd, EPOLLIN as u32)?;
        self.fd_map.insert(fd, FdKind::Listener(port));
        Ok(())
    }

    pub fn add_client(&mut self, fd: RawFd) -> Result<(), String> {
        self.add_fd(fd, EPOLLIN as u32)?;
        self.fd_map.insert(fd, FdKind::Client);
        Ok(())
    }

    pub fn set_writable(&self, fd: RawFd) -> Result<(), String> {
        self.mod_fd(fd, EPOLLOUT as u32)
    }

    pub fn set_readable(&self, fd: RawFd) -> Result<(), String> {
        self.mod_fd(fd, EPOLLIN as u32)
    }

    // Keep the fd registered (so HUP/ERR still surface) but stop reporting
    // EPOLLIN while a CGI response is pending for this client.
    pub fn set_idle(&self, fd: RawFd) -> Result<(), String> {
        self.mod_fd(fd, 0)
    }

    pub fn add_cgi_stdout(&mut self, fd: RawFd, client_fd: RawFd) -> Result<(), String> {
        self.add_fd(fd, EPOLLIN as u32)?;
        self.fd_map.insert(fd, FdKind::CgiStdout(client_fd));
        Ok(())
    }

    pub fn add_cgi_stdin(&mut self, fd: RawFd, client_fd: RawFd) -> Result<(), String> {
        self.add_fd(fd, EPOLLOUT as u32)?;
        self.fd_map.insert(fd, FdKind::CgiStdin(client_fd));
        Ok(())
    }

    pub fn remove(&mut self, fd: RawFd) {
        unsafe {
            epoll_ctl(self.epfd, EPOLL_CTL_DEL, fd, std::ptr::null_mut());
            libc::close(fd);
        }
        self.fd_map.remove(&fd);
    }

    pub fn wait(&self, events: &mut [epoll_event; MAX_EVENTS]) -> i32 {
        unsafe { epoll_wait(self.epfd, events.as_mut_ptr(), MAX_EVENTS as i32, TIMEOUT_MS) }
    }

    fn add_fd(&self, fd: RawFd, flags: u32) -> Result<(), String> {
        let mut ev = epoll_event {
            events: flags,
            u64: fd as u64,
        };
        let ret = unsafe { epoll_ctl(self.epfd, EPOLL_CTL_ADD, fd, &mut ev) };
        if ret < 0 {
            Err(format!("epoll_ctl ADD failed for fd {}: errno {}", fd, errno()))
        } else {
            Ok(())
        }
    }

    fn mod_fd(&self, fd: RawFd, flags: u32) -> Result<(), String> {
        let mut ev = epoll_event {
            events: flags,
            u64: fd as u64,
        };
        let ret = unsafe { epoll_ctl(self.epfd, EPOLL_CTL_MOD, fd, &mut ev) };
        if ret < 0 {
            Err(format!("epoll_ctl MOD failed for fd {}: errno {}", fd, errno()))
        } else {
            Ok(())
        }
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        unsafe { libc::close(self.epfd) };
    }
}

pub fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

pub fn set_nonblocking(fd: RawFd) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags < 0 {
        return Err(format!("fcntl F_GETFL failed: errno {}", errno()));
    }
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        Err(format!("fcntl F_SETFL failed: errno {}", errno()))
    } else {
        Ok(())
    }
}
