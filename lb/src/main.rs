use io_uring::{opcode, types, IoUring};
use std::ffi::CString;
use std::mem;
use std::os::unix::io::RawFd;
use std::ptr;
mod logging;

use logging::{Level, Category};

const CQE_F_MORE: u32 = 1 << 1;

fn main() -> std::io::Result<()> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let upstream_str = std::env::var("UPSTREAMS")
        .unwrap_or_else(|_| "/data/shared/api1.sock,/data/shared/api2.sock".to_string());

    let upstreams: Vec<String> = upstream_str.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if upstreams.is_empty() {
        logging::log(Level::Warn, Category::IoUring, "No upstreams provided in UPSTREAMS env var");
        std::process::exit(1);
    }

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
    let mut ring = IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(4096)?;

    let uds_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
    if uds_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Increase send buffer to handle high connection spikes
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

    eprintln!("Single-threaded io_uring LB listening on 0.0.0.0:9999 handing off to {} upstreams", up_addrs.len());
    logging::log(Level::Info, Category::IoUring, &format!("LB started, {} upstreams configured", up_addrs.len()));

    push_accept(&mut ring, listener_fd);

    let mut rr = 0;

    loop {
        ring.submit_and_wait(1)?;

        let mut completions = Vec::new();
        for cqe in ring.completion() {
            completions.push((cqe.result(), cqe.flags()));
        }

        for (res, flags) in completions {
            if (flags & CQE_F_MORE) == 0 {
                push_accept(&mut ring, listener_fd);
            }

            if res >= 0 {
                let client_fd = res as RawFd;
                logging::log(Level::Debug, Category::IoUring, &format!("Client accepted, fd={}", client_fd));
                
                set_tcp_nodelay(client_fd);

                let target_addr = &up_addrs[rr % up_addrs.len()];
                let upstream_idx = rr % up_addrs.len();
                rr = rr.wrapping_add(1);

                logging::log(Level::Debug, Category::Request, &format!("Handing off fd={} to upstream {}", client_fd, upstream_idx));
                send_fd(uds_fd, target_addr, client_fd);

                unsafe { libc::close(client_fd); }
                logging::log(Level::Debug, Category::IoUring, &format!("Local close after handoff, fd={}", client_fd));
            }
        }
    }
}

fn send_fd(sock: libc::c_int, addr: &libc::sockaddr_un, fd_to_send: libc::c_int) {
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
        }

        // Fast non-blocking send with minimal retries
        let mut retries = 0;
        loop {
            let res = libc::sendmsg(sock, &msg, 0);
            if res >= 0 {
                logging::log(Level::Debug, Category::Request, &format!("FD handoff successful, fd={}", fd_to_send));
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                retries += 1;
                if retries > 50 {
                    logging::log(Level::Warn, Category::Request, &format!("FD handoff failed after {} retries (WouldBlock), fd={}", retries, fd_to_send));
                    break;
                }
                std::thread::yield_now();
            } else {
                logging::log(Level::Warn, Category::Request, &format!("FD handoff error: {}, fd={}", err, fd_to_send));
                break;
            }
        }
    }
}

fn push_accept(ring: &mut IoUring, listen_fd: RawFd) {
    let sqe = opcode::AcceptMulti::new(types::Fd(listen_fd))
        .build()
        .user_data(1);
    loop {
        unsafe {
            if ring.submission().push(&sqe).is_ok() {
                return;
            }
        }
        let _ = ring.submit();
    }
}

fn create_listener(port: u16, backlog: i32) -> std::io::Result<RawFd> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    
    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, &one as *const _ as *const libc::c_void, mem::size_of::<libc::c_int>() as libc::socklen_t);
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, &one as *const _ as *const libc::c_void, mem::size_of::<libc::c_int>() as libc::socklen_t);
    }

    let mut addr: libc::sockaddr_in = unsafe { mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = port.to_be();
    addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();
    
    let rc = unsafe {
        libc::bind(fd, &addr as *const _ as *const libc::sockaddr, mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
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
