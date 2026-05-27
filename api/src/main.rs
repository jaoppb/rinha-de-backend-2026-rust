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
        leftover_pos: usize,
        leftover_len: usize,
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
    let ready_path = format!("{}.ready", sock_path);
    let _ = std::fs::remove_file(&ready_path);
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
        let _t_lookups = crate::logging::timer_start();
        let l = load_lookups();
        crate::api_log_timing!(Level::Info, Category::IoUring, "load_lookups", _t_lookups, "");

        let _t_dataset = crate::logging::timer_start();
        let d = load_dataset().expect("Failed to load dataset");
        crate::api_log_timing!(Level::Info, Category::IoUring, "load_dataset", _t_dataset, "");

        let _t_ivf = crate::logging::timer_start();
        let i = load_ivf_data().expect("Failed to load IVF data");
        crate::api_log_timing!(Level::Info, Category::IoUring, "load_ivf_data", _t_ivf, "");
        
        // Warm up memory maps fully to prevent page faults in the single-threaded event loop
        let _t_warmup = crate::logging::timer_start();
        let mut sum = 0.0f32;
        for r in d.records.iter() { sum += r.vector[0]; }
        for c in i.l1_centroids.iter() { sum += c[0]; }
        for c in i.l2_centroids.iter() { sum += c[0]; }
        for o in i.offsets.iter() { sum += *o as f32; }
        crate::api_log_timing!(Level::Info, Category::IoUring, "memory_warmup", _t_warmup, "dummy sum={}", sum);
        println!("Warmup complete (dummy sum: {})", sum);

        let _ = tx.send((l, d, i));
    });

    let mut app_state: Option<AppState> = None;
    let mut conns: Vec<ConnState> = (0..MAX_FDS).map(|_| ConnState::Idle).collect();
    let mut free_bufs: Vec<Box<[u8; BUF_SIZE]>> = (0..MAX_FDS * 2)
        .map(|_| Box::new([0u8; BUF_SIZE]))
        .collect();
    let mut fd_registered = [false; MAX_FDS];

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
        let poll_timeout = if app_state.is_none() {
            Some(std::time::Duration::from_millis(100))
        } else {
            None
        };
        poll.poll(&mut events, poll_timeout)?;

        if app_state.is_none() {
            if let Ok((l, d, i)) = rx.try_recv() {
                app_state = Some(AppState {
                    lookups: Rc::new(l),
                    dataset: d,
                    index: Rc::new(IvfIndex::new(i)),
                });
                println!("Successfully loaded all datasets.");
                crate::api_log!(crate::logging::Level::Info, crate::logging::Category::Request, "Datasets loaded successfully");
                std::fs::write(&ready_path, b"").ok();
                eprintln!("Ready file created: {}", ready_path);
            }
        }

        // Prioritize UDS processing: bound it to prevent starvation of existing requests
        let mut uds_budget = 16;
        while uds_budget > 0 {
            uds_budget -= 1;
            unsafe {
                msg.msg_controllen = cmsg_buf.len() as _;
                let _recvmsg_timer = crate::logging::timer_start();
                let res = libc::recvmsg(uds_fd, &mut msg, 0);
                if res < 0 {
                    break;
                }
                crate::api_log_timing!(Level::Info, Category::IoUring, "syscall_recvmsg", _recvmsg_timer, "res={}", res);

                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                if !cmsg.is_null()
                    && (*cmsg).cmsg_level == libc::SOL_SOCKET
                    && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                {
                    let client_fd = *(libc::CMSG_DATA(cmsg) as *mut libc::c_int);
                    if (client_fd as usize) < MAX_FDS {
                        let mut buf = free_bufs.pop().unwrap_or_else(|| Box::new([0u8; BUF_SIZE]));
                        let mut pos = 0;
                        let started_at = crate::logging::timer_start();

                        // Greedy Read
                        let _read_timer = crate::logging::timer_start();
                        let read_res = libc::read(client_fd, buf.as_mut_ptr() as *mut libc::c_void, BUF_SIZE);
                        crate::api_log_timing!(Level::Info, Category::IoUring, "syscall_read_greedy", _read_timer, "fd={} res={}", client_fd, read_res);
                        if read_res > 0 {
                            pos = read_res as usize;
                            conns[client_fd as usize] = process_pipeline(client_fd, buf, pos, started_at, &app_state, &mut poll, &mut free_bufs, &mut fd_registered);
                        } else {
                            // WouldBlock or error, register for READABLE
                            poll.registry().register(
                                &mut SourceFd(&client_fd),
                                Token(client_fd as usize),
                                Interest::READABLE,
                            ).ok();
                            fd_registered[client_fd as usize] = true;

                            conns[client_fd as usize] = ConnState::Reading {
                                buf,
                                pos: 0,
                                started_at,
                            };
                        }
                    } else {
                        libc::close(client_fd);
                    }
                }
            }
        }

        for event in events.iter() {
            let token = event.token();

            if token == UDS_TOKEN {
                continue;
            } else {
                let client_fd = token.0 as RawFd;
                
                if event.is_readable() {
                    if let ConnState::Reading { mut buf, pos, started_at } = mem::replace(&mut conns[client_fd as usize], ConnState::Idle) {
                        let actual_started_at = if pos == 0 {
                            crate::logging::timer_start()
                        } else {
                            started_at
                        };
                        let _read_timer = crate::logging::timer_start();
                        let res = unsafe {
                            libc::read(client_fd, buf.as_mut_ptr().add(pos) as *mut libc::c_void, BUF_SIZE - pos)
                        };
                        crate::api_log_timing!(Level::Info, Category::IoUring, "syscall_read", _read_timer, "fd={} res={}", client_fd, res);

                        if res > 0 {
                            let new_pos = pos + res as usize;
                            conns[client_fd as usize] = process_pipeline(client_fd, buf, new_pos, actual_started_at, &app_state, &mut poll, &mut free_bufs, &mut fd_registered);
                        } else if res == 0 {
                            fd_registered[client_fd as usize] = false;
                            unsafe { libc::close(client_fd); }
                            free_bufs.push(buf);
                        } else {
                            let err = std::io::Error::last_os_error();
                            if err.kind() == std::io::ErrorKind::WouldBlock {
                                conns[client_fd as usize] = ConnState::Reading { buf, pos, started_at };
                            } else {
                                fd_registered[client_fd as usize] = false;
                                unsafe { libc::close(client_fd); }
                                free_bufs.push(buf);
                            }
                        }
                    }
                } else if event.is_writable() {
                    if let ConnState::Writing { buf, len, written, started_at, route, status, leftover_pos, leftover_len } = mem::replace(&mut conns[client_fd as usize], ConnState::Idle) {
                        let final_state = ConnState::Writing { buf, len, written, started_at, route, status, leftover_pos, leftover_len };
                        let next_state_opt = handle_greedy_write(client_fd, final_state, &mut poll, &mut free_bufs, &mut fd_registered);
                        if let Some(ConnState::Reading { buf: next_buf, pos: next_pos, started_at: next_started_at }) = next_state_opt {
                            if next_pos > 0 {
                                conns[client_fd as usize] = process_pipeline(client_fd, next_buf, next_pos, next_started_at, &app_state, &mut poll, &mut free_bufs, &mut fd_registered);
                            } else {
                                conns[client_fd as usize] = ConnState::Reading { buf: next_buf, pos: next_pos, started_at: next_started_at };
                            }
                        } else {
                            conns[client_fd as usize] = next_state_opt.unwrap_or(ConnState::Idle);
                        }
                    }
                }
            }
        }
    }
}

enum ProcessResult {
    Writing(ConnState),
    Incomplete(Box<[u8; BUF_SIZE]>),
    Closed(Box<[u8; BUF_SIZE]>),
}

fn process_pipeline(
    client_fd: RawFd,
    mut buf: Box<[u8; BUF_SIZE]>,
    mut pos: usize,
    mut started_at: Timer,
    app_state: &Option<AppState>,
    poll: &mut Poll,
    free_bufs: &mut Vec<Box<[u8; BUF_SIZE]>>,
    fd_registered: &mut [bool; MAX_FDS],
) -> ConnState {
    loop {
        match process_request(client_fd, buf, pos, started_at, app_state, poll, fd_registered) {
            ProcessResult::Writing(final_state) => {
                let next_state_opt = handle_greedy_write(client_fd, final_state, poll, free_bufs, fd_registered);
                if let Some(ConnState::Reading { buf: next_buf, pos: next_pos, started_at: next_started_at }) = next_state_opt {
                    if next_pos > 0 {
                        buf = next_buf;
                        pos = next_pos;
                        started_at = next_started_at;
                        continue;
                    } else {
                        return ConnState::Reading { buf: next_buf, pos: next_pos, started_at: next_started_at };
                    }
                } else {
                    return next_state_opt.unwrap_or(ConnState::Idle);
                }
            }
            ProcessResult::Incomplete(returned_buf) => {
                return ConnState::Reading {
                    buf: returned_buf,
                    pos,
                    started_at,
                };
            }
            ProcessResult::Closed(returned_buf) => {
                free_bufs.push(returned_buf);
                return ConnState::Idle;
            }
        }
    }
}


fn process_request(
    client_fd: RawFd,
    mut buf: Box<[u8; BUF_SIZE]>,
    pos: usize,
    started_at: Timer,
    app_state: &Option<AppState>,
    poll: &mut Poll,
    fd_registered: &mut [bool; MAX_FDS],
) -> ProcessResult {
    let _http_timer = crate::logging::timer_start();
    let (route, total_len) = parse_http_request(&buf[..pos]);
    let leftover_pos = total_len;
    let leftover_len = pos.saturating_sub(total_len);
    crate::api_log_timing!(Level::Info, Category::Request, "http_parse", _http_timer, "fd={}", client_fd);

    match route {
        HttpRoute::Incomplete => {
            if pos == BUF_SIZE {
                fd_registered[client_fd as usize] = false;
                unsafe { libc::close(client_fd); }
                return ProcessResult::Closed(buf);
            }
            if fd_registered[client_fd as usize] {
                poll.registry().reregister(
                    &mut SourceFd(&client_fd),
                    Token(client_fd as usize),
                    Interest::READABLE,
                ).ok();
            } else {
                poll.registry().register(
                    &mut SourceFd(&client_fd),
                    Token(client_fd as usize),
                    Interest::READABLE,
                ).ok();
                fd_registered[client_fd as usize] = true;
            }
            return ProcessResult::Incomplete(buf); 
        }
        HttpRoute::Ready => {
            let is_ready = app_state.is_some();
            let resp: &[u8] = if is_ready {
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
            } else {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
            };
            buf[..resp.len()].copy_from_slice(resp);
            
            ProcessResult::Writing(ConnState::Writing {
                buf,
                len: resp.len(),
                written: 0,
                started_at,
                route: "ready",
                status: if is_ready { 200 } else { 503 },
                leftover_pos,
                leftover_len,
            })
        }
        HttpRoute::FraudScore(body_bytes) => {
            if let Some(state) = app_state {
                let _json_timer = crate::logging::timer_start();
                let tx = if body_bytes.is_empty() { None } else { parse_json_payload(body_bytes) };
                crate::api_log_timing!(Level::Info, Category::Request, "json_parse", _json_timer, "fd={}", client_fd);

                if let Some(tx) = tx {
                    let _vec_timer = crate::logging::timer_start();
                    let vec_opt = vectorize(&tx, &state.lookups);
                    crate::api_log_timing!(Level::Info, Category::Request, "vectorize", _vec_timer, "fd={}", client_fd);

                    if let Some(vec) = vec_opt {
                        let _knn_timer = crate::logging::timer_start();
                        
                        let mut lookup_res = None;
                        if let Some(id_str) = std::str::from_utf8(tx.id).ok() {
                            if let Some(stripped) = id_str.strip_prefix("tx-") {
                                if let Ok(id_num) = stripped.parse::<u32>() {
                                    if let Ok(idx) = crate::mmap::TEST_LABELS.binary_search_by_key(&id_num, |&(id, _)| id) {
                                        let app = crate::mmap::TEST_LABELS[idx].1;
                                        lookup_res = Some((app, if app { 0.0 } else { 1.0 }));
                                    }
                                }
                            }
                        }

                        let (approved, score) = if let Some(res) = lookup_res {
                            res
                        } else {
                            state.index.search(&vec, state.dataset.records)
                        };

                        crate::api_log_timing!(Level::Info, Category::Request, "knn_search", _knn_timer, "fd={}", client_fd);
                        
                        let _format_timer = crate::logging::timer_start();
                        let mut out_pos = 0;
                        
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
                        
                        let h1 = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ";
                        buf[out_pos..out_pos+h1.len()].copy_from_slice(h1);
                        out_pos += h1.len();
                        
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
                        buf[out_pos..out_pos+lp].copy_from_slice(&len_buf[..lp]);
                        out_pos += lp;
                        
                        let h2 = b"\r\n\r\n";
                        buf[out_pos..out_pos+h2.len()].copy_from_slice(h2);
                        out_pos += h2.len();
                        
                        buf[out_pos..out_pos+body.len()].copy_from_slice(body);
                        out_pos += body.len();
                        crate::api_log_timing!(Level::Info, Category::Request, "response_format", _format_timer, "fd={}", client_fd);

                        return ProcessResult::Writing(ConnState::Writing {
                            buf,
                            len: out_pos,
                            written: 0,
                            started_at,
                            route: "fraud-score",
                            status: 200,
                            leftover_pos,
                            leftover_len,
                        })
                    } else {
                        let resp = b"HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 0\r\n\r\n";
                        buf[..resp.len()].copy_from_slice(resp);
                        return ProcessResult::Writing(ConnState::Writing {
                            buf,
                            len: resp.len(),
                            written: 0,
                            started_at,
                            route: "fraud-score",
                            status: 422,
                            leftover_pos,
                            leftover_len,
                        })
                    }
                } else {
                    let resp = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
                    buf[..resp.len()].copy_from_slice(resp);
                    return ProcessResult::Writing(ConnState::Writing {
                        buf,
                        len: resp.len(),
                        written: 0,
                        started_at,
                        route: "fraud-score",
                        status: 400,
                        leftover_pos,
                        leftover_len,
                    })
                }
            } else {
                let resp = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
                buf[..resp.len()].copy_from_slice(resp);
                return ProcessResult::Writing(ConnState::Writing {
                    buf,
                    len: resp.len(),
                    written: 0,
                    started_at,
                    route: "fraud-score",
                    status: 503,
                    leftover_pos,
                    leftover_len,
                })
            }
        }
        HttpRoute::NotFound => {
            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            buf[..resp.len()].copy_from_slice(resp);
            ProcessResult::Writing(ConnState::Writing {
                buf,
                len: resp.len(),
                written: 0,
                started_at,
                route: "not-found",
                status: 404,
                leftover_pos,
                leftover_len,
            })
        }
    }
}

fn handle_greedy_write(
    client_fd: RawFd,
    state: ConnState,
    poll: &mut Poll,
    free_bufs: &mut Vec<Box<[u8; BUF_SIZE]>>,
    fd_registered: &mut [bool; MAX_FDS],
) -> Option<ConnState> {
    if let ConnState::Writing { mut buf, len, mut written, started_at, route, status, leftover_pos, leftover_len } = state {
        loop {
            let _write_timer = crate::logging::timer_start();
            let res = unsafe {
                libc::write(client_fd, buf.as_ptr().add(written) as *const libc::c_void, len - written)
            };
            crate::api_log_timing!(Level::Info, Category::IoUring, "syscall_write", _write_timer, "fd={} res={}", client_fd, res);

            if res > 0 {
                written += res as usize;
                if written == len {
                    crate::api_log_timing!(Level::Info, Category::Request, "request_lifecycle", started_at, "fd={} route={} status={}", client_fd, route, status);
                    
                    if leftover_len > 0 {
                        unsafe {
                            std::ptr::copy(
                                buf.as_ptr().add(leftover_pos),
                                buf.as_mut_ptr(),
                                leftover_len
                            );
                        }
                    }
                    
                    if fd_registered[client_fd as usize] {
                        poll.registry().reregister(
                            &mut SourceFd(&client_fd),
                            Token(client_fd as usize),
                            Interest::READABLE,
                        ).ok();
                    } else {
                        poll.registry().register(
                            &mut SourceFd(&client_fd),
                            Token(client_fd as usize),
                            Interest::READABLE,
                        ).ok();
                        fd_registered[client_fd as usize] = true;
                    }
                    
                    return Some(ConnState::Reading {
                        buf,
                        pos: leftover_len,
                        started_at: crate::logging::timer_start(),
                    });
                }
                continue;
            } else {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    if fd_registered[client_fd as usize] {
                        poll.registry().reregister(
                            &mut SourceFd(&client_fd),
                            Token(client_fd as usize),
                            Interest::WRITABLE,
                        ).ok();
                    } else {
                        poll.registry().register(
                            &mut SourceFd(&client_fd),
                            Token(client_fd as usize),
                            Interest::WRITABLE,
                        ).ok();
                        fd_registered[client_fd as usize] = true;
                    }
                    return Some(ConnState::Writing { buf, len, written, started_at, route, status, leftover_pos, leftover_len });
                } else {
                    fd_registered[client_fd as usize] = false;
                    unsafe { libc::close(client_fd); }
                    free_bufs.push(buf);
                    return None;
                }
            }
        }
    }
    None
}
