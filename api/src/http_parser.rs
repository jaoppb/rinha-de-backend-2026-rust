pub enum HttpRoute<'a> {
    Ready,
    FraudScore(&'a [u8]),
    NotFound,
}

pub fn parse_http_request(buf: &[u8]) -> HttpRoute<'_> {
    let mut i = 0;

    // Skip method
    while i < buf.len() && buf[i] != b' ' {
        i += 1;
    }
    if i >= buf.len() {
        return HttpRoute::NotFound;
    }
    i += 1;

    // Path
    let path_start = i;
    while i < buf.len() && buf[i] != b' ' {
        i += 1;
    }
    if i >= buf.len() {
        return HttpRoute::NotFound;
    }
    let path = &buf[path_start..i];

    if path == b"/ready" {
        return HttpRoute::Ready;
    }

    if path == b"/fraud-score" {
        // Find body (after \r\n\r\n)
        let body_sep = b"\r\n\r\n";
        let mut body_start = 0;
        for j in 0..buf.len().saturating_sub(3) {
            if &buf[j..j + 4] == body_sep {
                body_start = j + 4;
                break;
            }
        }

        if body_start == 0 || body_start >= buf.len() {
            return HttpRoute::FraudScore(&[]);
        }
        return HttpRoute::FraudScore(&buf[body_start..]);
    }

    HttpRoute::NotFound
}
