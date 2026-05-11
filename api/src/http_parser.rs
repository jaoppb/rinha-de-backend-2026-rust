pub enum HttpRoute<'a> {
    Ready,
    FraudScore(&'a [u8]),
    NotFound,
    Incomplete,
}

pub fn parse_http_request(buf: &[u8]) -> (HttpRoute<'_>, usize) {
    // 1. Find headers end (\r\n\r\n)
    let mut headers_end = 0;
    for j in 0..buf.len().saturating_sub(3) {
        if buf[j] == b'\r' && buf[j + 1] == b'\n' && buf[j + 2] == b'\r' && buf[j + 3] == b'\n' {
            headers_end = j + 4;
            break;
        }
    }

    if headers_end == 0 {
        return (HttpRoute::Incomplete, 0);
    }

    let headers = &buf[..headers_end];

    // 2. Extract path (between first and second space)
    let mut path: &[u8] = b"";
    let mut space_count = 0;
    let mut path_start = 0;
    for (idx, &b) in headers.iter().enumerate() {
        if b == b' ' {
            space_count += 1;
            if space_count == 1 {
                path_start = idx + 1;
            } else if space_count == 2 {
                path = &headers[path_start..idx];
                break;
            }
        }
        if b == b'\r' {
            break;
        }
    }

    // 3. Extract Content-Length
    let mut content_length = 0;
    let cl_header = b"content-length: "; // We should check case-insensitively or trust Nginx
    
    // Find Content-Length header
    // Manual search for simplicity and speed
    for j in 0..headers.len().saturating_sub(16) {
        // Match "content-length: " or "Content-Length: "
        if (headers[j] == b'c' || headers[j] == b'C') &&
           (headers[j+1] == b'o' || headers[j+1] == b'O') &&
           (headers[j+2] == b'n' || headers[j+2] == b'N') &&
           (headers[j+3] == b't' || headers[j+3] == b'T') &&
           (headers[j+4] == b'e' || headers[j+4] == b'E') &&
           (headers[j+5] == b'n' || headers[j+5] == b'N') &&
           (headers[j+6] == b't' || headers[j+6] == b'T') &&
           headers[j+7] == b'-' &&
           (headers[j+8] == b'l' || headers[j+8] == b'L') &&
           (headers[j+9] == b'e' || headers[j+9] == b'E') &&
           (headers[j+10] == b'n' || headers[j+10] == b'N') &&
           (headers[j+11] == b'g' || headers[j+11] == b'G') &&
           (headers[j+12] == b't' || headers[j+12] == b'T') &&
           (headers[j+13] == b'h' || headers[j+13] == b'H') &&
           headers[j+14] == b':' &&
           headers[j+15] == b' ' {
            
            let start = j + 16;
            let mut end = start;
            while end < headers.len() && headers[end] >= b'0' && headers[end] <= b'9' {
                end += 1;
            }
            if let Ok(s) = std::str::from_utf8(&headers[start..end]) {
                content_length = s.parse().unwrap_or(0);
            }
            break;
        }
    }

    let total_len = headers_end + content_length;
    if buf.len() < total_len {
        return (HttpRoute::Incomplete, 0);
    }

    let route = if path == b"/ready" {
        HttpRoute::Ready
    } else if path == b"/fraud-score" {
        HttpRoute::FraudScore(&buf[headers_end..total_len])
    } else {
        HttpRoute::NotFound
    };

    (route, total_len)
}
