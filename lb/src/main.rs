use io_uring::{opcode, types, IoUring};
use std::collections::VecDeque;
use std::os::unix::io::RawFd;
use std::ptr;

const MAX_CONNECTIONS: usize = 512;
const BUFFER_SIZE: usize = 16384;
const BACKENDS: [&[u8]; 2] = [b"/data/shared/api1.sock\0", b"/data/shared/api2.sock\0"];

#[derive(Clone, Copy)]
enum Token {
    Accept,
    BackendConnect { conn_idx: usize },
    ReadFromClient { conn_idx: usize },
    WriteToBackend { conn_idx: usize, n: usize },
    ReadFromBackend { conn_idx: usize },
    WriteToClient { conn_idx: usize, n: usize },
}

impl Token {
    fn to_u64(self) -> u64 {
        match self {
            Token::Accept => 0,
            Token::BackendConnect { conn_idx } => (1 << 32) | (conn_idx as u64),
            Token::ReadFromClient { conn_idx } => (2 << 32) | (conn_idx as u64),
            Token::WriteToBackend { conn_idx, n } => (3 << 32) | ((n as u64) << 16) | (conn_idx as u64),
            Token::ReadFromBackend { conn_idx } => (4 << 32) | (conn_idx as u64),
            Token::WriteToClient { conn_idx, n } => (5 << 32) | ((n as u64) << 16) | (conn_idx as u64),
        }
    }

    fn from_u64(val: u64) -> Self {
        let tag = (val >> 32) as u32;
        let conn_idx = (val & 0xFFFF) as usize;
        match tag {
            0 => Token::Accept,
            1 => Token::BackendConnect { conn_idx },
            2 => Token::ReadFromClient { conn_idx },
            3 => Token::WriteToBackend { conn_idx, n: ((val >> 16) & 0xFFFF) as usize },
            4 => Token::ReadFromBackend { conn_idx },
            5 => Token::WriteToClient { conn_idx, n: ((val >> 16) & 0xFFFF) as usize },
            _ => unreachable!(),
        }
    }
}

struct Connection {
    client_fd: RawFd,
    backend_fd: RawFd,
    client_buf: Box<[u8; BUFFER_SIZE]>,
    backend_buf: Box<[u8; BUFFER_SIZE]>,
    backend_addr: Box<libc::sockaddr_un>,
    active: bool,
    closing: bool,
    client_eof: bool,
    backend_eof: bool,
    pending_ops: usize,
}

impl Connection {
    fn reset(&mut self) {
        self.client_fd = -1;
        self.backend_fd = -1;
        self.active = false;
        self.closing = false;
        self.client_eof = false;
        self.backend_eof = false;
        self.pending_ops = 0;
    }
}

fn main() -> std::io::Result<()> {
    let mut ring = IoUring::new(1024).map_err(|e| {
        eprintln!("LB: Failed to initialize io_uring: {}", e);
        e
    })?;

    let mut connections: Vec<Connection> = (0..MAX_CONNECTIONS)
        .map(|_| Connection {
            client_fd: -1,
            backend_fd: -1,
            client_buf: Box::new([0u8; BUFFER_SIZE]),
            backend_buf: Box::new([0u8; BUFFER_SIZE]),
            backend_addr: Box::new(unsafe { std::mem::zeroed() }),
            active: false,
            closing: false,
            client_eof: false,
            backend_eof: false,
            pending_ops: 0,
        })
        .collect();
    let mut free_indices: VecDeque<usize> = (0..MAX_CONNECTIONS).collect();
    let mut backend_counter = 0;

    // Create listener
    let listener_fd = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        let val: libc::c_int = 1;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, &val as *const _ as *const _, std::mem::size_of::<libc::c_int>() as libc::socklen_t);
        
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        addr.sin_family = libc::AF_INET as libc::sa_family_t;
        addr.sin_port = 9999u16.to_be();
        addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();
        
        if libc::bind(fd, &addr as *const _ as *const _, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t) < 0 {
            let err = std::io::Error::last_os_error();
            eprintln!("Bind failed: {}", err);
            panic!("Bind failed");
        }
        libc::listen(fd, 1024);
        fd
    };

    eprintln!("Raw io_uring LB listening on 0.0.0.0:9999");

    // Initial accept
    let mut client_addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut client_addr_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    
    unsafe {
        let accept_e = opcode::Accept::new(types::Fd(listener_fd), &mut client_addr as *mut _ as *mut _, &mut client_addr_len)
            .build()
            .user_data(Token::Accept.to_u64());
        ring.submission().push(&accept_e).expect("queue full");
    }

    loop {
        ring.submit_and_wait(1)?;

        let mut completions = Vec::new();
        for cqe in ring.completion() {
            completions.push((cqe.user_data(), cqe.result()));
        }

        let mut sq = ring.submission();

        for (user_data, res) in completions {
            let token = Token::from_u64(user_data);

            if let Token::Accept = token {
                if res >= 0 {
                    let client_fd = res;
                    if let Some(conn_idx) = free_indices.pop_front() {
                        let conn = &mut connections[conn_idx];
                        conn.reset();
                        conn.client_fd = client_fd;
                        conn.active = true;

                        let backend_path = BACKENDS[backend_counter % BACKENDS.len()];
                        backend_counter += 1;

                        unsafe {
                            let b_fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                            conn.backend_fd = b_fd;

                            conn.backend_addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
                            let path_len = backend_path.len();
                            let copy_len = path_len.min(108);
                            ptr::copy_nonoverlapping(backend_path.as_ptr(), conn.backend_addr.sun_path.as_mut_ptr() as *mut u8, copy_len);

                            let addr_len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;

                            let connect_e = opcode::Connect::new(types::Fd(b_fd), &*conn.backend_addr as *const _ as *const _, addr_len)
                                .build()
                                .user_data(Token::BackendConnect { conn_idx }.to_u64());
                            if sq.push(&connect_e).is_ok() {
                                conn.pending_ops += 1;
                            }
                        }
                    } else {
                        unsafe { libc::close(client_fd); }
                    }
                }
                // Re-arm accept
                unsafe {
                    let accept_e = opcode::Accept::new(types::Fd(listener_fd), &mut client_addr as *mut _ as *mut _, &mut client_addr_len)
                        .build()
                        .user_data(Token::Accept.to_u64());
                    sq.push(&accept_e).ok();
                }
                continue;
            }

            // Connection-based tokens
            let conn_idx = match token {
                Token::BackendConnect { conn_idx } => conn_idx,
                Token::ReadFromClient { conn_idx } => conn_idx,
                Token::WriteToBackend { conn_idx, .. } => conn_idx,
                Token::ReadFromBackend { conn_idx } => conn_idx,
                Token::WriteToClient { conn_idx, .. } => conn_idx,
                _ => unreachable!(),
            };

            let conn = &mut connections[conn_idx];
            conn.pending_ops -= 1;

            if conn.closing {
                if conn.pending_ops == 0 && conn.active {
                    unsafe { libc::close(conn.client_fd); libc::close(conn.backend_fd); }
                    conn.active = false;
                    free_indices.push_back(conn_idx);
                }
                continue;
            }

            match token {
                Token::BackendConnect { .. } => {
                    if res >= 0 {
                        unsafe {
                            let r_client = opcode::Read::new(types::Fd(conn.client_fd), conn.client_buf.as_ptr() as *mut _, BUFFER_SIZE as u32)
                                .build()
                                .user_data(Token::ReadFromClient { conn_idx }.to_u64());
                            if sq.push(&r_client).is_ok() { conn.pending_ops += 1; }

                            let r_backend = opcode::Read::new(types::Fd(conn.backend_fd), conn.backend_buf.as_ptr() as *mut _, BUFFER_SIZE as u32)
                                .build()
                                .user_data(Token::ReadFromBackend { conn_idx }.to_u64());
                            if sq.push(&r_backend).is_ok() { conn.pending_ops += 1; }
                        }
                    } else {
                        // Backend connect failed
                        unsafe { libc::close(conn.client_fd); libc::close(conn.backend_fd); }
                        conn.closing = true;
                        conn.active = false;
                        if conn.pending_ops == 0 {
                            free_indices.push_back(conn_idx);
                        }
                    }
                }
                Token::ReadFromClient { .. } => {
                    if res > 0 {
                        let n = res as usize;
                        unsafe {
                            let w_backend = opcode::Write::new(types::Fd(conn.backend_fd), conn.client_buf.as_ptr(), n as u32)
                                .build()
                                .user_data(Token::WriteToBackend { conn_idx, n }.to_u64());
                            if sq.push(&w_backend).is_ok() { conn.pending_ops += 1; }
                        }
                    } else if res == 0 {
                        // Client EOF
                        conn.client_eof = true;
                        if !conn.backend_eof {
                            unsafe { libc::shutdown(conn.backend_fd, libc::SHUT_WR); }
                        }
                        if conn.backend_eof {
                            conn.closing = true;
                        }
                    } else {
                        // Error from client
                        conn.closing = true;
                    }
                }
                Token::WriteToBackend { .. } => {
                    if res >= 0 {
                        if !conn.client_eof {
                            unsafe {
                                let r_client = opcode::Read::new(types::Fd(conn.client_fd), conn.client_buf.as_ptr() as *mut _, BUFFER_SIZE as u32)
                                    .build()
                                    .user_data(Token::ReadFromClient { conn_idx }.to_u64());
                                if sq.push(&r_client).is_ok() { conn.pending_ops += 1; }
                            }
                        }
                    } else {
                        conn.closing = true;
                    }
                }
                Token::ReadFromBackend { .. } => {
                    if res > 0 {
                        let n = res as usize;
                        unsafe {
                            let w_client = opcode::Write::new(types::Fd(conn.client_fd), conn.backend_buf.as_ptr(), n as u32)
                                .build()
                                .user_data(Token::WriteToClient { conn_idx, n }.to_u64());
                            if sq.push(&w_client).is_ok() { conn.pending_ops += 1; }
                        }
                    } else if res == 0 {
                        // Backend EOF
                        conn.backend_eof = true;
                        if !conn.client_eof {
                            unsafe { libc::shutdown(conn.client_fd, libc::SHUT_WR); }
                        }
                        if conn.client_eof {
                            conn.closing = true;
                        }
                    } else {
                        // Error from backend
                        conn.closing = true;
                    }
                }
                Token::WriteToClient { .. } => {
                    if res >= 0 {
                        if !conn.backend_eof {
                            unsafe {
                                let r_backend = opcode::Read::new(types::Fd(conn.backend_fd), conn.backend_buf.as_ptr() as *mut _, BUFFER_SIZE as u32)
                                    .build()
                                    .user_data(Token::ReadFromBackend { conn_idx }.to_u64());
                                if sq.push(&r_backend).is_ok() { conn.pending_ops += 1; }
                            }
                        }
                    } else {
                        conn.closing = true;
                    }
                }
                Token::Accept => unreachable!(),
            }

            if conn.closing && conn.pending_ops == 0 && conn.active {
                unsafe { libc::close(conn.client_fd); libc::close(conn.backend_fd); }
                conn.active = false;
                free_indices.push_back(conn_idx);
            }
        }
    }
}
