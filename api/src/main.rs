mod http_parser;
mod json_parser;
mod knn;
mod mmap;
mod vectorizer;

use std::mem;
use std::os::unix::io::RawFd;
use std::ptr;
use io_uring::{IoUring, opcode, types};
use std::rc::Rc;

use crate::http_parser::{HttpRoute, parse_http_request};
use crate::json_parser::parse_json_payload;
use crate::knn::IvfIndex;
use crate::mmap::{load_dataset, load_ivf_data, load_lookups};
use crate::vectorizer::vectorize;

const RING_SIZE: u32 = 4096;
const BUF_SIZE: usize = 2048;

fn main() -> std::io::Result<()> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let lookups = Rc::new(load_lookups());
    let dataset = load_dataset().expect("Failed to load dataset");
    let ivf_data = load_ivf_data().expect("Failed to load IVF data");
    println!("Successfully loaded all datasets.");
    let index = Rc::new(IvfIndex::new(ivf_data));
    let records = dataset.records;

    let sock_path = std::env::var("SOCK").expect("SOCK env var must be set");
    let _ = std::fs::remove_file(&sock_path);
    let uds_fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        if fd < 0 { return Err(std::io::Error::last_os_error()); }
        
        let mut addr: libc::sockaddr_un = mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path_bytes = sock_path.as_bytes();
        let len = std::cmp::min(path_bytes.len(), addr.sun_path.len() - 1);
        ptr::copy_nonoverlapping(path_bytes.as_ptr(), addr.sun_path.as_mut_ptr() as *mut u8, len);
        
        if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, mem::size_of::<libc::sockaddr_un>() as libc::socklen_t) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        
        let rcvbuf: libc::c_int = 16 * 1024 * 1024;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, &rcvbuf as *const _ as *const libc::c_void, mem::size_of::<libc::c_int>() as libc::socklen_t);
        
        fd
    };

    let mut ring = IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(RING_SIZE)?;

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

    push_recvmsg(&mut ring, uds_fd, &mut msg);

    loop {
        ring.submit_and_wait(1)?;

        let mut cqes_data = Vec::with_capacity(64);
        for cqe in ring.completion() {
            cqes_data.push((cqe.result(), cqe.user_data()));
        }

        for (res, user_data) in cqes_data {
            if user_data == 0 { // RecvMsg
                if res >= 0 {
                    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
                    if !cmsg.is_null() && unsafe { (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS } {
                        let fd = unsafe { *(libc::CMSG_DATA(cmsg) as *mut libc::c_int) };
                        current_pos = 0;
                        push_read(&mut ring, fd, read_buf.as_mut_ptr(), BUF_SIZE);
                    } else {
                        push_recvmsg(&mut ring, uds_fd, &mut msg);
                    }
                } else {
                    push_recvmsg(&mut ring, uds_fd, &mut msg);
                }
            } else if (user_data & 0x1) == 1 { // Read
                let fd = (user_data >> 1) as RawFd;
                if res > 0 {
                    current_pos += res as usize;
                    let (route, _) = parse_http_request(&read_buf[..current_pos]);
                    match route {
                        HttpRoute::Incomplete => {
                            if current_pos < BUF_SIZE {
                                unsafe {
                                    push_read(&mut ring, fd, read_buf.as_mut_ptr().add(current_pos), BUF_SIZE - current_pos);
                                }
                            } else {
                                push_close(&mut ring, fd);
                            }
                        }
                        HttpRoute::Ready => {
                            let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                            write_buf[..resp.len()].copy_from_slice(resp);
                            push_write(&mut ring, fd, write_buf.as_ptr(), resp.len());
                        }
                        HttpRoute::FraudScore(body) => {
                            let tx = (!body.is_empty()).then(|| parse_json_payload(body)).flatten();
                            let response = match tx.as_ref().map(|t| vectorize(t, &lookups)) {
                                Some(Some(vec)) => {
                                    let (approved, score) = index.search(&vec, records);
                                    let resp_body = format!("{{\"approved\":{},\"fraud_score\":{:.1}}}", approved, score);
                                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", resp_body.len(), resp_body)
                                }
                                Some(None) => "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n".to_string(),
                                None => "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n".to_string(),
                            };
                            let b = response.as_bytes();
                            let len = std::cmp::min(b.len(), BUF_SIZE);
                            write_buf[..len].copy_from_slice(&b[..len]);
                            push_write(&mut ring, fd, write_buf.as_ptr(), len);
                        }
                        HttpRoute::NotFound => {
                            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                            write_buf[..resp.len()].copy_from_slice(resp);
                            push_write(&mut ring, fd, write_buf.as_ptr(), resp.len());
                        }
                    }
                } else {
                    push_close(&mut ring, fd);
                }
            } else if (user_data & 0x2) == 2 { // Write
                let fd = (user_data >> 2) as RawFd;
                push_close(&mut ring, fd);
            } else if (user_data & 0x4) == 4 { // Close
                // After closing, we are ready for the next client
                push_recvmsg(&mut ring, uds_fd, &mut msg);
            }
        }
    }
}

fn push_recvmsg(ring: &mut IoUring, fd: RawFd, msg: *mut libc::msghdr) {
    let sqe = opcode::RecvMsg::new(types::Fd(fd), msg)
        .build()
        .user_data(0);
    unsafe { ring.submission().push(&sqe).ok(); }
}

fn push_read(ring: &mut IoUring, fd: RawFd, buf: *mut u8, len: usize) {
    let sqe = opcode::Read::new(types::Fd(fd), buf, len as u32)
        .build()
        .user_data(((fd as u64) << 1) | 1);
    unsafe { ring.submission().push(&sqe).ok(); }
}

fn push_write(ring: &mut IoUring, fd: RawFd, buf: *const u8, len: usize) {
    let sqe = opcode::Write::new(types::Fd(fd), buf, len as u32)
        .build()
        .user_data(((fd as u64) << 2) | 2);
    unsafe { ring.submission().push(&sqe).ok(); }
}

fn push_close(ring: &mut IoUring, fd: RawFd) {
    let sqe = opcode::Close::new(types::Fd(fd))
        .build()
        .user_data(((fd as u64) << 3) | 4);
    unsafe { ring.submission().push(&sqe).ok(); }
}
