use mio::{Events, Poll, Token, Interest};
use mio::unix::SourceFd;
use std::ffi::CString;
use std::mem;
use std::os::unix::io::RawFd;
use std::ptr;
mod logging;

use logging::{Category, Level};

const LISTENER: Token = Token(0);

struct Upstream {
    fd: RawFd,
    queue: [RawFd; 1024],
    head: usize,
    tail: usize,
    registered_writable: bool,
}

impl Upstream {
    fn new(path: &str) -> std::io::Result<Self> {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK, 0) };
        if fd < 0 { return Err(std::io::Error::last_os_error()); }
        let sndbuf: libc::c_int = 16 * 1024 * 1024;
        unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, &sndbuf as *const _ as *const libc::c_void, mem::size_of::<libc::c_int>() as libc::socklen_t);
        }

        let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let c_path = CString::new(path).unwrap();
        let path_bytes = c_path.as_bytes();
        let copy_len = path_bytes.len().min(addr.sun_path.len() - 1);
        unsafe {
            ptr::copy_nonoverlapping(path_bytes.as_ptr(), addr.sun_path.as_mut_ptr() as *mut u8, copy_len);
        }

        // We use connect so EPOLLOUT correctly tracks the target's receive queue
        unsafe {
            libc::connect(fd, &addr as *const _ as *const libc::sockaddr, mem::size_of::<libc::sockaddr_un>() as libc::socklen_t);
        }

        Ok(Self {
            fd,
            queue: [0; 1024],
            head: 0,
            tail: 0,
            registered_writable: false,
        })
    }

    fn push(&mut self, client_fd: RawFd) -> bool {
        let next_tail = (self.tail + 1) % 1024;
        if next_tail == self.head { return false; } // full
        self.queue[self.tail] = client_fd;
        self.tail = next_tail;
        true
    }
    
    fn pop(&mut self) -> Option<RawFd> {
        if self.head == self.tail { return None; }
        let fd = self.queue[self.head];
        self.head = (self.head + 1) % 1024;
        Some(fd)
    }
    
    fn peek(&self) -> Option<RawFd> {
        if self.head == self.tail { return None; }
        Some(self.queue[self.head])
    }
    
    fn is_empty(&self) -> bool {
        self.head == self.tail
    }
}

fn main() -> std::io::Result<()> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let upstream_str = std::env::var("UPSTREAMS")
        .unwrap_or_else(|_| "/data/shared/api1.sock,/data/shared/api2.sock".to_string());

    let upstream_list: Vec<String> = upstream_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if upstream_list.is_empty() {
        logging::log(
            Level::Warn,
            Category::IoUring,
            "No upstreams provided in UPSTREAMS env var",
        );
        std::process::exit(1);
    }

    let mut upstreams = Vec::with_capacity(upstream_list.len());
    for ups in upstream_list {
        upstreams.push(Upstream::new(&ups)?);
    }

    let listener_fd = create_listener(9999, 8192)?;
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);

    poll.registry().register(
        &mut SourceFd(&listener_fd),
        LISTENER,
        Interest::READABLE,
    )?;

    eprintln!(
        "Single-threaded mio LB listening on 0.0.0.0:9999 handing off to {} upstreams (Async Queuing)",
        upstreams.len()
    );
    logging::log(
        Level::Info,
        Category::IoUring,
        &format!("LB started, {} upstreams configured", upstreams.len()),
    );

    let mut rr = 0;

    loop {
        poll.poll(&mut events, None)?;

        for event in events.iter() {
            let token = event.token();
            
            if token == LISTENER {
                let mut accept_budget = 128;
                loop {
                    if accept_budget == 0 {
                        break;
                    }
                    accept_budget -= 1;

                    let mut client_addr: libc::sockaddr_in = unsafe { mem::zeroed() };
                    let mut client_addr_len = mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
                    
                    let client_fd = unsafe {
                        libc::accept4(
                            listener_fd,
                            &mut client_addr as *mut _ as *mut libc::sockaddr,
                            &mut client_addr_len,
                            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                        )
                    };

                    if client_fd < 0 {
                        let err = std::io::Error::last_os_error();
                        if err.kind() == std::io::ErrorKind::WouldBlock {
                            break;
                        }
                        continue;
                    }

                    let accept_to_handoff_timer = logging::timer_start();
                    set_tcp_nodelay(client_fd);

                    let target_idx = rr % upstreams.len();
                    rr = rr.wrapping_add(1);
                    
                    let upstream = &mut upstreams[target_idx];
                    let mut handoff_ok = false;
                    let mut queued = false;
                    
                    if upstream.is_empty() {
                        match try_send_fd(upstream.fd, client_fd) {
                            Ok(()) => {
                                handoff_ok = true;
                            }
                            Err(e) => {
                                if e.raw_os_error() == Some(libc::EWOULDBLOCK) || e.raw_os_error() == Some(libc::EAGAIN) {
                                    // Fall through to push
                                } else {
                                    send_service_unavailable(client_fd);
                                    unsafe { libc::close(client_fd); }
                                    continue;
                                }
                            }
                        }
                    }
                    
                    if handoff_ok {
                        logging::log_timing(
                            Level::Info,
                            Category::Request,
                            "accept_to_handoff",
                            accept_to_handoff_timer,
                            format_args!("fd={} upstream={} result=ok", client_fd, target_idx),
                        );
                        unsafe { libc::close(client_fd); }
                    } else {
                        if upstream.push(client_fd) {
                            queued = true;
                            if !upstream.registered_writable {
                                poll.registry().register(
                                    &mut SourceFd(&upstream.fd),
                                    Token(target_idx + 1),
                                    Interest::WRITABLE,
                                ).ok();
                                upstream.registered_writable = true;
                            }
                        } else {
                            send_service_unavailable(client_fd);
                            unsafe { libc::close(client_fd); }
                        }
                        
                        logging::log_timing(
                            if queued { Level::Info } else { Level::Warn },
                            Category::Request,
                            "accept_to_handoff",
                            accept_to_handoff_timer,
                            format_args!("fd={} upstream={} result={}", client_fd, target_idx, if queued { "queued" } else { "queue_full" }),
                        );
                    }
                }
            } else {
                // Writable event on an upstream
                let target_idx = token.0 - 1;
                let upstream = &mut upstreams[target_idx];
                
                while let Some(client_fd) = upstream.peek() {
                    match try_send_fd(upstream.fd, client_fd) {
                        Ok(()) => {
                            upstream.pop();
                            unsafe { libc::close(client_fd); }
                        }
                        Err(e) => {
                            if e.raw_os_error() == Some(libc::EWOULDBLOCK) || e.raw_os_error() == Some(libc::EAGAIN) {
                                break;
                            } else {
                                upstream.pop();
                                send_service_unavailable(client_fd);
                                unsafe { libc::close(client_fd); }
                            }
                        }
                    }
                }
                
                if upstream.is_empty() && upstream.registered_writable {
                    poll.registry().deregister(&mut SourceFd(&upstream.fd)).ok();
                    upstream.registered_writable = false;
                }
            }
        }
    }
}

fn try_send_fd(sock: libc::c_int, fd_to_send: libc::c_int) -> Result<(), std::io::Error> {
    unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_name = ptr::null_mut();
        msg.msg_namelen = 0;

        let mut iov = libc::iovec {
            iov_base: "1".as_ptr() as *mut libc::c_void,
            iov_len: 1,
        };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;

        let mut cmsg_buf = [0u8; 24]; // CMSG_SPACE(sizeof(int))
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = 24;

        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if !cmsg.is_null() {
            (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<libc::c_int>() as u32) as _;
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            let data = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
            *data = fd_to_send;
            msg.msg_controllen = (*cmsg).cmsg_len;
        } else {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        }

        let res = libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL);
        if res >= 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

fn send_service_unavailable(client_fd: RawFd) {
    const RESPONSE: &[u8] =
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    unsafe {
        libc::send(client_fd, RESPONSE.as_ptr() as *const libc::c_void, RESPONSE.len(), libc::MSG_NOSIGNAL);
    }
}

fn create_listener(port: u16, backlog: i32) -> std::io::Result<RawFd> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &one as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &one as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    let mut addr: libc::sockaddr_in = unsafe { mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = port.to_be();
    addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();

    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd); }
        return Err(err);
    }

    if unsafe { libc::listen(fd, backlog) } < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd); }
        return Err(err);
    }

    Ok(fd)
}

fn set_tcp_nodelay(fd: RawFd) {
    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &one as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}
