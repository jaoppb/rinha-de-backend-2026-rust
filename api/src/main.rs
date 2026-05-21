mod http_parser;
mod json_parser;
mod knn;
mod logging;
mod mmap;
mod vectorizer;

use mio::{Events, Interest, Poll, Token};
use mio::unix::SourceFd;
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

const BUF_SIZE: usize = 16 * 1024;
const MAX_FDS: usize = 1024;
const UDS_TOKEN: Token = Token(2048);

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
        
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

        fd
    };

    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);

    poll.registry().register(
        &mut SourceFd(&uds_fd),
        UDS_TOKEN,
        Interest::READABLE,
    )?;

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

    loop {
        poll.poll(&mut events, None)?;

        if app_state.is_none() {
            if let Ok((l, d, i)) = rx.try_recv() {
                app_state = Some(AppState {
                    lookups: Rc::new(l),
                    dataset: d,
                    index: Rc::new(IvfIndex::new(i)),
                });
                println!("Successfully loaded all datasets.");
                crate::api_log!(Level::Info, Category::Request, "Datasets loaded successfully");
            }
        }

        for event in events.iter() {
            let token = event.token();

            if token == UDS_TOKEN {
                loop {
                    unsafe {
                        msg.msg_controllen = cmsg_buf.len() as _;
                        let res = libc::recvmsg(uds_fd, &mut msg, 0);
                        if res < 0 {
                            break;
                        }

                        let cmsg = libc::CMSG_FIRSTHDR(&msg);
                        if !cmsg.is_null()
                            && (*cmsg).cmsg_level == libc::SOL_SOCKET
                            && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                        {
                            let client_fd = *(libc::CMSG_DATA(cmsg) as *mut libc::c_int);
                            if (client_fd as usize) < MAX_FDS {
                                let flags = libc::fcntl(client_fd, libc::F_GETFL, 0);
                                libc::fcntl(client_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

                                poll.registry().register(
                                    &mut SourceFd(&client_fd),
                                    Token(client_fd as usize),
                                    Interest::READABLE,
                                ).ok();

                                conns[client_fd as usize] = ConnState::Reading {
                                    buf: Box::new([0u8; BUF_SIZE]),
                                    pos: 0,
                                    started_at: logging::timer_start(),
                                };
                            } else {
                                libc::close(client_fd);
                            }
                        }
                    }
                }
            } else {
                let client_fd = token.0 as RawFd;
                
                if event.is_readable() {
                    let mut should_close = false;
                    if let ConnState::Reading { mut buf, mut pos, started_at } = mem::replace(&mut conns[client_fd as usize], ConnState::Idle) {
                        loop {
                            let res = unsafe {
                                libc::read(client_fd, buf.as_mut_ptr().add(pos) as *mut libc::c_void, BUF_SIZE - pos)
                            };

                            if res > 0 {
                                pos += res as usize;
                                let (route, _) = parse_http_request(&buf[..pos]);
                                match route {
                                    HttpRoute::Incomplete => {
                                        if pos == BUF_SIZE {
                                            should_close = true;
                                            break;
                                        }
                                        continue;
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
                                        
                                        poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                        conns[client_fd as usize] = ConnState::Writing {
                                            buf: w_buf,
                                            len: resp.len(),
                                            written: 0,
                                            started_at,
                                            route: "ready",
                                            status: if is_ready { 200 } else { 503 },
                                        };
                                        break;
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
                                        
                                        poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                        conns[client_fd as usize] = ConnState::Writing {
                                            buf: w_buf,
                                            len,
                                            written: 0,
                                            started_at,
                                            route: "fraud-score",
                                            status,
                                        };
                                        break;
                                    }
                                    HttpRoute::NotFound => {
                                        let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                                        let mut w_buf = Box::new([0u8; BUF_SIZE]);
                                        w_buf[..resp.len()].copy_from_slice(resp);
                                        poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                        conns[client_fd as usize] = ConnState::Writing {
                                            buf: w_buf,
                                            len: resp.len(),
                                            written: 0,
                                            started_at,
                                            route: "not-found",
                                            status: 404,
                                        };
                                        break;
                                    }
                                }
                            } else if res == 0 {
                                should_close = true;
                                break;
                            } else {
                                let err = std::io::Error::last_os_error();
                                if err.kind() == std::io::ErrorKind::WouldBlock {
                                    conns[client_fd as usize] = ConnState::Reading { buf, pos, started_at };
                                } else {
                                    should_close = true;
                                }
                                break;
                            }
                        }
                    }
                    if should_close {
                        unsafe { libc::close(client_fd); }
                        conns[client_fd as usize] = ConnState::Idle;
                    }
                } else if event.is_writable() {
                    let mut should_close = false;
                    if let ConnState::Writing { buf, len, mut written, started_at, route, status } = mem::replace(&mut conns[client_fd as usize], ConnState::Idle) {
                        loop {
                            let res = unsafe {
                                libc::write(client_fd, buf.as_ptr().add(written) as *const libc::c_void, len - written)
                            };

                            if res > 0 {
                                written += res as usize;
                                if written == len {
                                    crate::api_log_timing!(Level::Info, Category::Request, "request_lifecycle", started_at, "fd={} route={} status={}", client_fd, route, status);
                                    should_close = true;
                                    break;
                                }
                                continue;
                            } else {
                                let err = std::io::Error::last_os_error();
                                if err.kind() == std::io::ErrorKind::WouldBlock {
                                    conns[client_fd as usize] = ConnState::Writing { buf, len, written, started_at, route, status };
                                } else {
                                    should_close = true;
                                }
                                break;
                            }
                        }
                    }
                    if should_close {
                        unsafe { libc::close(client_fd); }
                        conns[client_fd as usize] = ConnState::Idle;
                    }
                }
            }
        }
    }
}
