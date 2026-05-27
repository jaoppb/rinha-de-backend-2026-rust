use mio::{Events, Poll, Token, Interest};
use mio::unix::SourceFd;
use std::ffi::CString;
use std::mem;
use std::os::unix::io::RawFd;
use std::ptr;
mod logging;

use logging::{Category, Level};

const LISTENER: Token = Token(0);

fn main() -> std::io::Result<()> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let upstream_str = std::env::var("UPSTREAMS")
        .unwrap_or_else(|_| "/data/shared/api1.sock,/data/shared/api2.sock".to_string());

    let upstreams: Vec<String> = upstream_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if upstreams.is_empty() {
        logging::log(
            Level::Warn,
            Category::IoUring,
            "No upstreams provided in UPSTREAMS env var",
        );
        std::process::exit(1);
    }

    let ready_paths: Vec<String> = upstreams.iter().map(|s| format!("{}.ready", s)).collect();

    let mut up_addrs = Vec::with_capacity(upstreams.len());
    for ups in upstreams {
        let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let c_path = CString::new(ups.as_str()).unwrap();
        let path_bytes = c_path.as_bytes();
        let copy_len = path_bytes.len().min(addr.sun_path.len() - 1);
        unsafe {
            ptr::copy_nonoverlapping(
                path_bytes.as_ptr(),
                addr.sun_path.as_mut_ptr() as *mut u8,
                copy_len,
            );
        }
        up_addrs.push(addr);
    }

    let listener_fd = create_listener(9999, 8192)?;
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);

    poll.registry().register(
        &mut SourceFd(&listener_fd),
        LISTENER,
        Interest::READABLE,
    )?;

    let uds_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK, 0) };
    if uds_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let sndbuf: libc::c_int = 16 * 1024 * 1024;
    unsafe {
        libc::setsockopt(
            uds_fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &sndbuf as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    eprintln!(
        "Single-threaded mio LB listening on 0.0.0.0:9999 handing off to {} upstreams",
        up_addrs.len()
    );
    logging::log(
        Level::Info,
        Category::IoUring,
        &format!("LB started, {} upstreams configured", up_addrs.len()),
    );

    let mut rr = 0;

    // Wait for all upstreams to signal readiness
    eprintln!("Waiting for {} upstreams to be ready...", ready_paths.len());
    loop {
        let all_ready = ready_paths.iter().all(|p| std::path::Path::new(p).exists());
        if all_ready {
            eprintln!("All upstreams ready!");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    loop {
        poll.poll(&mut events, None)?;

        for event in events.iter() {
            if event.token() == LISTENER {
                loop {
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

                    let request_start = std::time::Instant::now();
                    let accept_to_handoff_timer = logging::timer_start();
                    set_tcp_nodelay(client_fd);

                    let target_addr = &up_addrs[rr % up_addrs.len()];
                    let upstream_idx = rr % up_addrs.len();
                    rr = rr.wrapping_add(1);

                    let handoff_ok = send_fd(uds_fd, target_addr, client_fd, request_start);
                    logging::log_timing(
                        if handoff_ok { Level::Info } else { Level::Warn },
                        Category::Request,
                        "accept_to_handoff",
                        accept_to_handoff_timer,
                        format_args!(
                            "fd={} upstream={} result={}",
                            client_fd,
                            upstream_idx,
                            if handoff_ok { "ok" } else { "fallback_503" }
                        ),
                    );

                    if !handoff_ok {
                        send_service_unavailable(client_fd);
                    }

                    unsafe {
                        libc::close(client_fd);
                    }
                }
            }
        }
    }
}

fn send_fd(sock: libc::c_int, addr: &libc::sockaddr_un, fd_to_send: libc::c_int, start: std::time::Instant) -> bool {
    unsafe {
        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_name = addr as *const _ as *mut libc::c_void;
        msg.msg_namelen = mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;

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
            return false;
        }

        loop {
            let res = libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL);
            if res >= 0 {
                return true;
            }
            let err = std::io::Error::last_os_error().raw_os_error();
            if err == Some(libc::EWOULDBLOCK) || err == Some(libc::EAGAIN) {
                if start.elapsed() >= std::time::Duration::from_millis(2) {
                    break;
                }
                std::thread::yield_now();
                continue;
            }
            break;
        }
        false
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
        unsafe {
            libc::close(fd);
        }
        return Err(err);
    }

    if unsafe { libc::listen(fd, backlog) } < 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
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
