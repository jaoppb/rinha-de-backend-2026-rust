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
use crate::logging::{Category, Level};
use crate::mmap::{load_dataset, load_ivf_data, load_lookups};
use crate::vectorizer::vectorize;

const RING_SIZE: u32 = 4096;
const BUF_SIZE: usize = 16 * 1024;

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

    let mut state: Option<(
        Rc<crate::mmap::LookupData>,
        crate::mmap::Dataset,
        Rc<IvfIndex>,
    )> = None;

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

    let mut read_buf = vec![0u8; BUF_SIZE];
    let mut write_buf = vec![0u8; BUF_SIZE];
    let mut current_pos = 0;
    let mut request_started_at: Option<logging::Timer> = None;
    let mut pending_write_timing: Option<(RawFd, logging::Timer, &'static str, u16)> = None;

    push_recvmsg(&mut ring, uds_fd, &mut msg);

    loop {
        ring.submit_and_wait(1)?;

        if state.is_none() {
            if let Ok((l, d, i)) = rx.try_recv() {
                state = Some((Rc::new(l), d, Rc::new(IvfIndex::new(i))));
                println!("Successfully loaded all datasets.");
                logging::log(
                    Level::Info,
                    Category::Request,
                    "Datasets loaded successfully",
                );
            }
        }

        let mut cqes_data = Vec::with_capacity(64);
        for cqe in ring.completion() {
            cqes_data.push((cqe.result(), cqe.user_data()));
        }

        for (res, user_data) in cqes_data {
            if user_data == 0 {
                // RecvMsg
                if res >= 0 {
                    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
                    if !cmsg.is_null()
                        && unsafe {
                            (*cmsg).cmsg_level == libc::SOL_SOCKET
                                && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                        }
                    {
                        let fd = unsafe { *(libc::CMSG_DATA(cmsg) as *mut libc::c_int) };
                        current_pos = 0;
                        request_started_at = Some(logging::timer_start());
                        pending_write_timing = None;
                        push_read(&mut ring, fd, read_buf.as_mut_ptr(), BUF_SIZE);
                    } else {
                        push_recvmsg(&mut ring, uds_fd, &mut msg);
                    }
                } else {
                    push_recvmsg(&mut ring, uds_fd, &mut msg);
                }
            } else if (user_data & 0x1) == 1 {
                // Read
                let fd = (user_data >> 1) as RawFd;
                if res > 0 {
                    current_pos += res as usize;
                    logging::log(
                        Level::Debug,
                        Category::IoUring,
                        &format!("Read {} bytes, total buffer pos: {}", res, current_pos),
                    );
                    let parse_timer = logging::timer_start();
                    let (route, _) = parse_http_request(&read_buf[..current_pos]);
                    let route_name = match &route {
                        HttpRoute::Ready => "ready",
                        HttpRoute::FraudScore(_) => "fraud-score",
                        HttpRoute::NotFound => "not-found",
                        HttpRoute::Incomplete => "incomplete",
                    };
                    logging::log_timing(
                        Level::Debug,
                        Category::Request,
                        "parse_http_request",
                        parse_timer,
                        format_args!("fd={} route={} bytes={}", fd, route_name, current_pos),
                    );
                    match route {
                        HttpRoute::Incomplete => {
                            logging::log(
                                Level::Debug,
                                Category::Request,
                                "Request incomplete, waiting for more data",
                            );
                            if current_pos < BUF_SIZE {
                                unsafe {
                                    push_read(
                                        &mut ring,
                                        fd,
                                        read_buf.as_mut_ptr().add(current_pos),
                                        BUF_SIZE - current_pos,
                                    );
                                }
                            } else {
                                logging::log(
                                    Level::Warn,
                                    Category::Request,
                                    "Buffer full, closing connection",
                                );
                                push_close(&mut ring, fd);
                            }
                        }
                        HttpRoute::Ready => {
                            logging::log(Level::Info, Category::Request, "Route: Ready");
                            let is_ready = state.is_some();
                            let resp: &[u8] = if is_ready {
                                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                            } else {
                                logging::log(
                                    Level::Warn,
                                    Category::Request,
                                    "Ready endpoint called but state not ready",
                                );
                                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
                            };
                            write_buf[..resp.len()].copy_from_slice(resp);
                            if let Some(started_at) = request_started_at.take() {
                                pending_write_timing = Some((
                                    fd,
                                    started_at,
                                    "ready",
                                    if is_ready { 200 } else { 503 },
                                ));
                            }
                            push_write(&mut ring, fd, write_buf.as_ptr(), resp.len());
                        }
                        HttpRoute::FraudScore(body) => {
                            logging::log(
                                Level::Info,
                                Category::Request,
                                &format!("Route: FraudScore, body_size: {}", body.len()),
                            );
                            if let Some((lookups, dataset, index)) = state.as_ref() {
                                let parse_json_timer = logging::timer_start();
                                let tx = if body.is_empty() {
                                    None
                                } else {
                                    parse_json_payload(body)
                                };
                                logging::log_timing(
                                    Level::Debug,
                                    Category::Request,
                                    "parse_json_payload",
                                    parse_json_timer,
                                    format_args!(
                                        "fd={} body_size={} parsed={}",
                                        fd,
                                        body.len(),
                                        tx.is_some()
                                    ),
                                );

                                let (response, status_code) = match tx {
                                    Some(tx) => {
                                        let vectorize_timer = logging::timer_start();
                                        let maybe_vec = vectorize(&tx, lookups);
                                        logging::log_timing(
                                            if maybe_vec.is_some() {
                                                Level::Debug
                                            } else {
                                                Level::Warn
                                            },
                                            Category::Request,
                                            "vectorize",
                                            vectorize_timer,
                                            format_args!(
                                                "fd={} result={}",
                                                fd,
                                                if maybe_vec.is_some() {
                                                    "ok"
                                                } else {
                                                    "invalid_input"
                                                }
                                            ),
                                        );

                                        if let Some(vec) = maybe_vec {
                                            let knn_timer = logging::timer_start();
                                            let (approved, score) =
                                                index.search(&vec, dataset.records);
                                            logging::log_timing(
                                                Level::Debug,
                                                Category::Request,
                                                "knn_search",
                                                knn_timer,
                                                format_args!(
                                                    "fd={} approved={} fraud_score={:.1}",
                                                    fd, approved, score
                                                ),
                                            );

                                            let build_response_timer = logging::timer_start();
                                            let resp_body = format!(
                                                "{{\"approved\":{},\"fraud_score\":{:.1}}}",
                                                approved, score
                                            );
                                            logging::log(
                                                Level::Debug,
                                                Category::Request,
                                                &format!(
                                                    "Fraud score response: approved={}, score={:.1}",
                                                    approved, score
                                                ),
                                            );
                                            let response = format!(
                                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                                resp_body.len(),
                                                resp_body
                                            );
                                            logging::log_timing(
                                                Level::Debug,
                                                Category::Request,
                                                "build_fraud_response",
                                                build_response_timer,
                                                format_args!(
                                                    "fd={} status=200 body_bytes={}",
                                                    fd,
                                                    resp_body.len()
                                                ),
                                            );
                                            (response, 200)
                                        } else {
                                            logging::log(
                                                Level::Debug,
                                                Category::Request,
                                                "Vectorization failed",
                                            );
                                            (
                                                "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n".to_string(),
                                                422,
                                            )
                                        }
                                    }
                                    None => {
                                        logging::log(
                                            Level::Debug,
                                            Category::Request,
                                            "JSON parsing failed",
                                        );
                                        (
                                            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n"
                                                .to_string(),
                                            400,
                                        )
                                    }
                                };
                                let b = response.as_bytes();
                                let len = std::cmp::min(b.len(), BUF_SIZE);
                                write_buf[..len].copy_from_slice(&b[..len]);
                                if let Some(started_at) = request_started_at.take() {
                                    pending_write_timing =
                                        Some((fd, started_at, "fraud-score", status_code));
                                }
                                push_write(&mut ring, fd, write_buf.as_ptr(), len);
                            } else {
                                logging::log(
                                    Level::Warn,
                                    Category::Request,
                                    "FraudScore endpoint called but state not ready",
                                );
                                let resp: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
                                write_buf[..resp.len()].copy_from_slice(resp);
                                if let Some(started_at) = request_started_at.take() {
                                    pending_write_timing =
                                        Some((fd, started_at, "fraud-score", 503));
                                }
                                push_write(&mut ring, fd, write_buf.as_ptr(), resp.len());
                            }
                        }
                        HttpRoute::NotFound => {
                            logging::log(Level::Info, Category::Request, "Route: NotFound (404)");
                            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                            write_buf[..resp.len()].copy_from_slice(resp);
                            if let Some(started_at) = request_started_at.take() {
                                pending_write_timing = Some((fd, started_at, "not-found", 404));
                            }
                            push_write(&mut ring, fd, write_buf.as_ptr(), resp.len());
                        }
                    }
                } else {
                    logging::log(
                        Level::Debug,
                        Category::IoUring,
                        "Read failed or connection closed",
                    );
                    if let Some(started_at) = request_started_at.take() {
                        logging::log_timing(
                            Level::Warn,
                            Category::Request,
                            "request_lifecycle",
                            started_at,
                            format_args!("fd={} route=read-failed status=closed", fd),
                        );
                    }
                    pending_write_timing = None;
                    push_close(&mut ring, fd);
                }
            } else if (user_data & 0x2) == 2 {
                // Write
                let fd = (user_data >> 2) as RawFd;
                if let Some((pending_fd, started_at, route, status)) = pending_write_timing.take() {
                    if pending_fd == fd {
                        logging::log_timing(
                            Level::Info,
                            Category::Request,
                            "request_lifecycle",
                            started_at,
                            format_args!("fd={} route={} status={}", fd, route, status),
                        );
                    } else {
                        pending_write_timing = Some((pending_fd, started_at, route, status));
                    }
                }
                push_close(&mut ring, fd);
            } else if (user_data & 0x4) == 4 {
                // Close
                let fd = (user_data >> 3) as RawFd;
                if let Some((pending_fd, _, _, _)) = pending_write_timing {
                    if pending_fd == fd {
                        pending_write_timing = None;
                    }
                }
                request_started_at = None;
                // After closing, we are ready for the next client
                push_recvmsg(&mut ring, uds_fd, &mut msg);
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
        .user_data(0);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}

fn push_read(ring: &mut IoUring, fd: RawFd, buf: *mut u8, len: usize) {
    let sqe = opcode::Read::new(types::Fd(fd), buf, len as u32)
        .build()
        .user_data(((fd as u64) << 1) | 1);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}

fn push_write(ring: &mut IoUring, fd: RawFd, buf: *const u8, len: usize) {
    let sqe = opcode::Write::new(types::Fd(fd), buf, len as u32)
        .build()
        .user_data(((fd as u64) << 2) | 2);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}

fn push_close(ring: &mut IoUring, fd: RawFd) {
    let sqe = opcode::Close::new(types::Fd(fd))
        .build()
        .user_data(((fd as u64) << 3) | 4);
    unsafe {
        ring.submission().push(&sqe).ok();
    }
}
