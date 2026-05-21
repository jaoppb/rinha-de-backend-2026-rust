mod http_parser;
mod json_parser;
mod knn;
mod logging;
mod mmap;
mod vectorizer;

use io_uring::{IoUring, opcode, types};
use std::mem;
use std::os::unix::io::RawFd;
use std::ptr;
use std::rc::Rc;

use crate::http_parser::{HttpRoute, parse_http_request};
use crate::json_parser::parse_json_payload;
use crate::knn::IvfIndex;
use crate::logging::{Category, Level, Timer};
use crate::mmap::{load_dataset, load_ivf_data, load_lookups};
use crate::vectorizer::vectorize;

const RING_SIZE: u32 = 4096;
const BUF_SIZE: usize = 16 * 1024;
const MAX_FDS: usize = 1024;

enum ConnState {
    Idle,
    Reading {
        buf: Box<[u8; BUF_SIZE]>,
        pos: usize,
        started_at: Timer,
    },
    Writing {
        buf: Box<[u8; BUF_SIZE]>,
        len: usize,
        written: usize,
        started_at: Timer,
        route: &'static str,
        status: u16,
    },
    Closing,
}

struct AppState {
    lookups: Rc<crate::mmap::LookupData>,
    dataset: crate::mmap::Dataset,
    index: Rc<IvfIndex>,
}

fn main() -> std::io::Result<()> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let sock_path = std::env::var("SOCK").expect("SOCK env var must be set");
    let _ = std::fs::remove_file(&sock_path);
    let uds_fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut addr: libc::sockaddr_un = mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path_bytes = sock_path.as_bytes();
        let len = std::cmp::min(path_bytes.len(), addr.sun_path.len() - 1);
        ptr::copy_nonoverlapping(
            path_bytes.as_ptr(),
            addr.sun_path.as_mut_ptr() as *mut u8,
            len,
        );

        if libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        ) < 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let rcvbuf: libc::c_int = 16 * 1024 * 1024;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        );

        fd
    };

    let mut ring = IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(RING_SIZE)?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let l = load_lookups();
        let d = load_dataset().expect("Failed to load dataset");
        let i = load_ivf_data().expect("Failed to load IVF data");
        let _ = tx.send((l, d, i));
    });

    let mut app_state: Option<AppState> = None;
    let mut conns: Vec<ConnState> = (0..MAX_FDS).map(|_| ConnState::Idle).collect();

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    let mut iov_base = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: iov_base.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let mut cmsg_buf = [0u8; 256];
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_buf.len() as _;

    // Initial RecvMsg to start accepting FDs
    push_recvmsg(&mut ring, uds_fd, &mut msg);

    loop {
        ring.submit_and_wait(1)?;

        if app_state.is_none() {
            if let Ok((l, d, i)) = rx.try_recv() {
                app_state = Some(AppState {
                    lookups: Rc::new(l),
                    dataset: d,
                    index: Rc::new(IvfIndex::new(i)),
                });
                println!("Successfully loaded all datasets.");
                api_log!(Level::Info, Category::Request, "Datasets loaded successfully");
            }
        }

        let mut cqes_data = Vec::with_capacity(64);
        for cqe in ring.completion() {
            cqes_data.push((cqe.result(), cqe.user_data()));
        }

        for (res, user_data) in cqes_data {
            let op = user_data & 0xF;
            let fd = (user_data >> 4) as RawFd;

            match op {
                0 => { // RecvMsg (New FD)
                    if res >= 0 {
                        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
                        if !cmsg.is_null()
                            && unsafe {
                                (*cmsg).cmsg_level == libc::SOL_SOCKET
                                    && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                            }
                        {
                            let client_fd = unsafe { *(libc::CMSG_DATA(cmsg) as *mut libc::c_int) };
                            if (client_fd as usize) < MAX_FDS {
                                let mut buf = Box::new([0u8; BUF_SIZE]);
                                let started_at = logging::timer_start();
                                push_read(&mut ring, client_fd, buf.as_mut_ptr(), BUF_SIZE);
                                conns[client_fd as usize] = ConnState::Reading {
                                    buf,
                                    pos: 0,
                                    started_at,
                                };
                            } else {
                                api_log!(Level::Warn, Category::Request, "FD {} exceeds MAX_FDS", client_fd);
                                unsafe { libc::close(client_fd); }
                            }
                        }
                    }
                    // Always repush RecvMsg to keep accepting
                    push_recvmsg(&mut ring, uds_fd, &mut msg);
                }
                1 => { // Read
                    if res > 0 {
                        let client_fd = fd;
                        if let ConnState::Reading { mut buf, mut pos, started_at } = mem::replace(&mut conns[client_fd as usize], ConnState::Idle) {
                            pos += res as usize;
                            let (route, _) = parse_http_request(&buf[..pos]);
                            
                            match route {
                                HttpRoute::Incomplete => {
                                    if pos < BUF_SIZE {
                                        push_read_at(&mut ring, client_fd, buf.as_mut_ptr(), BUF_SIZE, pos);
                                        conns[client_fd as usize] = ConnState::Reading { buf, pos, started_at };
                                    } else {
                                        api_log!(Level::Warn, Category::Request, "Buffer full for fd {}", client_fd);
                                        push_close(&mut ring, client_fd);
                                        conns[client_fd as usize] = ConnState::Closing;
                                    }
                                }
                                HttpRoute::Ready => {
                                    let is_ready = app_state.is_some();
                                    let resp: &[u8] = if is_ready {
                                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                                    } else {
                                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
                                    };
                                    let mut w_buf = Box::new([0u8; BUF_SIZE]);
                                    w_buf[..resp.len()].copy_from_slice(resp);
                                    push_write(&mut ring, client_fd, w_buf.as_ptr(), resp.len());
                                    conns[client_fd as usize] = ConnState::Writing {
                                        buf: w_buf,
                                        len: resp.len(),
                                        written: 0,
                                        started_at,
                                        route: "ready",
                                        status: if is_ready { 200 } else { 503 },
                                    };
                                }
                                HttpRoute::FraudScore(body_bytes) => {
                                    let (resp_str, status) = if let Some(state) = &app_state {
                                        let tx = if body_bytes.is_empty() { None } else { parse_json_payload(body_bytes) };
                                        if let Some(tx) = tx {
                                            if let Some(vec) = vectorize(&tx, &state.lookups) {
                                                let (approved, score) = state.index.search(&vec, state.dataset.records);
                                                let resp_body = format!("{{\"approved\":{},\"fraud_score\":{:.1}}}", approved, score);
                                                (format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", resp_body.len(), resp_body), 200)
                                            } else {
                                                ("HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n".to_string(), 422)
                                            }
                                        } else {
                                            ("HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n".to_string(), 400)
                                        }
                                    } else {
                                        ("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_string(), 503)
                                    };
                                    
                                    let mut w_buf = Box::new([0u8; BUF_SIZE]);
                                    let b = resp_str.as_bytes();
                                    let len = std::cmp::min(b.len(), BUF_SIZE);
                                    w_buf[..len].copy_from_slice(&b[..len]);
                                    push_write(&mut ring, client_fd, w_buf.as_ptr(), len);
                                    conns[client_fd as usize] = ConnState::Writing {
                                        buf: w_buf,
                                        len,
                                        written: 0,
                                        started_at,
                                        route: "fraud-score",
                                        status,
                                    };
                                }
                                HttpRoute::NotFound => {
                                    let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                                    let mut w_buf = Box::new([0u8; BUF_SIZE]);
                                    w_buf[..resp.len()].copy_from_slice(resp);
                                    push_write(&mut ring, client_fd, w_buf.as_ptr(), resp.len());
                                    conns[client_fd as usize] = ConnState::Writing {
                                        buf: w_buf,
                                        len: resp.len(),
                                        written: 0,
                                        started_at,
                                        route: "not-found",
                                        status: 404,
                                    };
                                }
                            }
                        }
                    } else {
                        // Read failed or EOF
                        if (fd as usize) < MAX_FDS {
                            if let ConnState::Reading { started_at, .. } = mem::replace(&mut conns[fd as usize], ConnState::Idle) {
                                api_log_timing!(Level::Warn, Category::Request, "request_lifecycle", started_at, "fd={} route=read-failed status=closed", fd);
                            }
                        }
                        push_close(&mut ring, fd);
                        conns[fd as usize] = ConnState::Closing;
                    }
                }
                2 => { // Write
                    if res > 0 {
                        if (fd as usize) < MAX_FDS {
                            if let ConnState::Writing { buf, len, mut written, started_at, route, status } = mem::replace(&mut conns[fd as usize], ConnState::Idle) {
                                written += res as usize;
                                if written < len {
                                    push_write_at(&mut ring, fd, buf.as_ptr(), len, written);
                                    conns[fd as usize] = ConnState::Writing { buf, len, written, started_at, route, status };
                                } else {
                                    api_log_timing!(Level::Info, Category::Request, "request_lifecycle", started_at, "fd={} route={} status={}", fd, route, status);
                                    push_close(&mut ring, fd);
                                    conns[fd as usize] = ConnState::Closing;
                                }
                            }
                        }
                    } else {
                        push_close(&mut ring, fd);
                        conns[fd as usize] = ConnState::Closing;
                    }
                }
                3 => { // Close
                    if (fd as usize) < MAX_FDS {
                        conns[fd as usize] = ConnState::Idle;
                    }
                }
                _ => {}
            }
        }
    }
}

fn push_recvmsg(ring: &mut IoUring, fd: RawFd, msg: *mut libc::msghdr) {
    unsafe {
        (*msg).msg_controllen = 256;
    }
    let sqe = opcode::RecvMsg::new(types::Fd(fd), msg)
        .build()
        .user_data(0); // OP 0, FD 0
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}

fn push_read(ring: &mut IoUring, fd: RawFd, buf: *mut u8, len: usize) {
    let sqe = opcode::Read::new(types::Fd(fd), buf, len as u32)
        .build()
        .user_data(((fd as u64) << 4) | 1);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}

fn push_read_at(ring: &mut IoUring, fd: RawFd, buf: *mut u8, total_len: usize, pos: usize) {
    let sqe = opcode::Read::new(types::Fd(fd), unsafe { buf.add(pos) }, (total_len - pos) as u32)
        .build()
        .user_data(((fd as u64) << 4) | 1);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}

fn push_write(ring: &mut IoUring, fd: RawFd, buf: *const u8, len: usize) {
    let sqe = opcode::Write::new(types::Fd(fd), buf, len as u32)
        .build()
        .user_data(((fd as u64) << 4) | 2);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}

fn push_write_at(ring: &mut IoUring, fd: RawFd, buf: *const u8, total_len: usize, written: usize) {
    let sqe = opcode::Write::new(types::Fd(fd), unsafe { buf.add(written) }, (total_len - written) as u32)
        .build()
        .user_data(((fd as u64) << 4) | 2);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}

fn push_close(ring: &mut IoUring, fd: RawFd) {
    let sqe = opcode::Close::new(types::Fd(fd))
        .build()
        .user_data(((fd as u64) << 4) | 3);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}
