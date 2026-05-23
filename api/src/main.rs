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
        
        // Warm up memory maps
        let mut sum = 0.0f32;
        for r in d.records.iter().take(1000) { sum += r.vector[0]; }
        for c in i.centroids.iter() { sum += c[0]; }
        for o in i.offsets.iter() { sum += *o as f32; }
        println!("Warmup complete (dummy sum: {})", sum);

        let _ = tx.send((l, d, i));
    });

    let mut app_state: Option<AppState> = None;
    let mut conns: Vec<ConnState> = (0..MAX_FDS).map(|_| ConnState::Idle).collect();
    let mut free_bufs: Vec<Box<[u8; BUF_SIZE]>> = (0..MAX_FDS * 2)
        .map(|_| Box::new([0u8; BUF_SIZE]))
        .collect();

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
                crate::api_log!(crate::logging::Level::Info, crate::logging::Category::Request, "Datasets loaded successfully");
            }
        }

        // Prioritize UDS processing: drain it completely before processing other events
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
                            buf: free_bufs.pop().unwrap_or_else(|| Box::new([0u8; BUF_SIZE])),
                            pos: 0,
                            started_at: crate::logging::timer_start(),
                        };
                    } else {
                        libc::close(client_fd);
                    }
                }
            }
        }

        for event in events.iter() {
            let token = event.token();

            if token == UDS_TOKEN {
                // Handled above for priority
                continue;
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
                                let http_timer = crate::logging::timer_start();
                                let (route, _) = parse_http_request(&buf[..pos]);
                                crate::api_log_timing!(Level::Info, Category::Request, "http_parse", http_timer, "fd={}", client_fd);
                                match route {
                                    HttpRoute::Incomplete => {
                                        if pos == BUF_SIZE {
                                            should_close = true;
                                            free_bufs.push(buf);
                                            break;
                                        }
                                        conns[client_fd as usize] = ConnState::Reading { buf, pos, started_at };
                                        break;
                                    }
                                    HttpRoute::Ready => {
                                        let is_ready = app_state.is_some();
                                        let resp: &[u8] = if is_ready {
                                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                                        } else {
                                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
                                        };
                                        buf[..resp.len()].copy_from_slice(resp);
                                        
                                        poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                        conns[client_fd as usize] = ConnState::Writing {
                                            buf,
                                            len: resp.len(),
                                            written: 0,
                                            started_at,
                                            route: "ready",
                                            status: if is_ready { 200 } else { 503 },
                                        };
                                        break;
                                    }
                                    HttpRoute::FraudScore(body_bytes) => {
                                        if let Some(state) = &app_state {
                                            let json_timer = crate::logging::timer_start();
                                            let tx = if body_bytes.is_empty() { None } else { parse_json_payload(body_bytes) };
                                            crate::api_log_timing!(Level::Info, Category::Request, "json_parse", json_timer, "fd={}", client_fd);

                                            if let Some(tx) = tx {
                                                let vec_timer = crate::logging::timer_start();
                                                let vec_opt = vectorize(&tx, &state.lookups);
                                                crate::api_log_timing!(Level::Info, Category::Request, "vectorize", vec_timer, "fd={}", client_fd);

                                                if let Some(vec) = vec_opt {
                                                    let knn_timer = crate::logging::timer_start();
                                                    let (approved, score) = state.index.search(&vec, state.dataset.records);
                                                    crate::api_log_timing!(Level::Info, Category::Request, "knn_search", knn_timer, "fd={}", client_fd);
                                                    
                                                    let mut pos = 0;
                                                    
                                                    // Construct JSON body manually to avoid format!
                                                    let mut body_buf = [0u8; 64];
                                                    let mut bp = 0;
                                                    let body_start = b"{\"approved\":";
                                                    body_buf[bp..bp+body_start.len()].copy_from_slice(body_start);
                                                    bp += body_start.len();
                                                    let app_str: &[u8] = if approved { b"true" } else { b"false" };
                                                    body_buf[bp..bp+app_str.len()].copy_from_slice(app_str);
                                                    bp += app_str.len();
                                                    let score_mid = b",\"fraud_score\":";
                                                    body_buf[bp..bp+score_mid.len()].copy_from_slice(score_mid);
                                                    bp += score_mid.len();
                                                    
                                                    // Simple float to string (1 decimal place as required: 0.0)
                                                    let s10 = (score * 10.0 + 0.5) as u32;
                                                    let whole = s10 / 10;
                                                    let frac = s10 % 10;
                                                    
                                                    if whole >= 10 {
                                                        body_buf[bp] = b'0' + (whole / 10) as u8;
                                                        bp += 1;
                                                    }
                                                    body_buf[bp] = b'0' + (whole % 10) as u8;
                                                    bp += 1;
                                                    body_buf[bp] = b'.';
                                                    bp += 1;
                                                    body_buf[bp] = b'0' + frac as u8;
                                                    bp += 1;
                                                    body_buf[bp] = b'}';
                                                    bp += 1;
                                                    
                                                    let body = &body_buf[..bp];
                                                    
                                                    // Construct HTTP response
                                                    let h1 = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ";
                                                    buf[pos..pos+h1.len()].copy_from_slice(h1);
                                                    pos += h1.len();
                                                    
                                                    let mut len_buf = [0u8; 8];
                                                    let mut lp = 0;
                                                    let mut n = bp;
                                                    if n == 0 {
                                                        len_buf[0] = b'0';
                                                        lp = 1;
                                                    } else {
                                                        let mut temp = [0u8; 8];
                                                        let mut tp = 0;
                                                        while n > 0 {
                                                            temp[tp] = b'0' + (n % 10) as u8;
                                                            n /= 10;
                                                            tp += 1;
                                                        }
                                                        while tp > 0 {
                                                            tp -= 1;
                                                            len_buf[lp] = temp[tp];
                                                            lp += 1;
                                                        }
                                                    }
                                                    buf[pos..pos+lp].copy_from_slice(&len_buf[..lp]);
                                                    pos += lp;
                                                    
                                                    let h2 = b"\r\n\r\n";
                                                    buf[pos..pos+h2.len()].copy_from_slice(h2);
                                                    pos += h2.len();
                                                    
                                                    buf[pos..pos+body.len()].copy_from_slice(body);
                                                    pos += body.len();

                                                    poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                                    conns[client_fd as usize] = ConnState::Writing {
                                                        buf,
                                                        len: pos,
                                                        written: 0,
                                                        started_at,
                                                        route: "fraud-score",
                                                        status: 200,
                                                    };
                                                } else {
                                                    let resp = b"HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n";
                                                    buf[..resp.len()].copy_from_slice(resp);
                                                    poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                                    conns[client_fd as usize] = ConnState::Writing {
                                                        buf,
                                                        len: resp.len(),
                                                        written: 0,
                                                        started_at,
                                                        route: "fraud-score",
                                                        status: 422,
                                                    };
                                                }
                                            } else {
                                                let resp = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                                                buf[..resp.len()].copy_from_slice(resp);
                                                poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                                conns[client_fd as usize] = ConnState::Writing {
                                                    buf,
                                                    len: resp.len(),
                                                    written: 0,
                                                    started_at,
                                                    route: "fraud-score",
                                                    status: 400,
                                                };
                                            }
                                        } else {
                                            let resp = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
                                            buf[..resp.len()].copy_from_slice(resp);
                                            poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                            conns[client_fd as usize] = ConnState::Writing {
                                                buf,
                                                len: resp.len(),
                                                written: 0,
                                                started_at,
                                                route: "fraud-score",
                                                status: 503,
                                            };
                                        };
                                        break;
                                    }

                                    HttpRoute::NotFound => {
                                        let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                                        buf[..resp.len()].copy_from_slice(resp);
                                        poll.registry().reregister(&mut SourceFd(&client_fd), token, Interest::WRITABLE).ok();
                                        conns[client_fd as usize] = ConnState::Writing {
                                            buf,
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
                                free_bufs.push(buf);
                                break;
                            } else {
                                let err = std::io::Error::last_os_error();
                                if err.kind() == std::io::ErrorKind::WouldBlock {
                                    conns[client_fd as usize] = ConnState::Reading { buf, pos, started_at };
                                } else {
                                    should_close = true;
                                    free_bufs.push(buf);
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
                                    free_bufs.push(buf);
                                    break;
                                }
                                continue;
                            } else {
                                let err = std::io::Error::last_os_error();
                                if err.kind() == std::io::ErrorKind::WouldBlock {
                                    conns[client_fd as usize] = ConnState::Writing { buf, len, written, started_at, route, status };
                                } else {
                                    should_close = true;
                                    free_bufs.push(buf);
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
