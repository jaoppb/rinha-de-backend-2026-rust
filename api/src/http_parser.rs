use memchr::memmem;

const MAX_REQUEST_SIZE: usize = 16 * 1024;

pub enum HttpRoute<'a> {
    Ready,
    FraudScore(&'a [u8]),
    NotFound,
    Incomplete,
}

pub fn parse_http_request(buf: &[u8]) -> (HttpRoute<'_>, usize) {
    // 1. Find headers end (\r\n\r\n) using optimized memmem
    let headers_end = match memmem::find(buf, b"\r\n\r\n") {
        Some(pos) => pos + 4,
        None => return (HttpRoute::Incomplete, 0),
    };

    let headers = &buf[..headers_end];

    // 2. Extract path (between first and second space)
    let mut path: &[u8] = b"";
    if let Some(first_space) = memchr::memchr(b' ', headers) {
        let after_first = &headers[first_space + 1..];
        if let Some(second_space) = memchr::memchr(b' ', after_first) {
            path = &after_first[..second_space];
        }
    }

    // 3. Extract Content-Length
    let mut content_length = 0;

    // Fast search for Content-Length (checking both common case variations)
    let cl_pos = memmem::find(headers, b"Content-Length: ")
        .or_else(|| memmem::find(headers, b"content-length: "));

    if let Some(pos) = cl_pos {
        let start = pos + 16;
        let mut val: usize = 0;
        let mut found = false;
        for &b in &headers[start..] {
            if b.is_ascii_digit() {
                val = val * 10 + (b - b'0') as usize;
                found = true;
            } else if b == b'\r' || b == b'\n' {
                break;
            } else if found {
                // Unexpected character after digits but before CRLF
                break;
            }
        }
        content_length = val;
    }

    let total_len = headers_end + content_length;
    if total_len > MAX_REQUEST_SIZE {
        return (HttpRoute::NotFound, 0);
    }
    if buf.len() < total_len {
        return (HttpRoute::Incomplete, 0);
    }

    // Accept exact "/ready" and common variants ("/ready/", "/ready?...")
    let route = if path.starts_with(b"/ready")
        && (path.len() == 6 || path.get(6) == Some(&b'/') || path.get(6) == Some(&b'?'))
    {
        HttpRoute::Ready
    } else if path.starts_with(b"/fraud-score") {
        HttpRoute::FraudScore(&buf[headers_end..total_len])
    } else {
        HttpRoute::NotFound
    };

    (route, total_len)
}
