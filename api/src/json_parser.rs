use memchr::{memchr, memchr2, memchr3};

pub struct ParsedTransaction<'a> {
    pub id: &'a [u8],
    pub amount: f32,
    pub installments: u8,
    pub requested_at: &'a [u8],
    pub customer_avg_amount: f32,
    pub customer_tx_count_24h: u32,
    pub customer_known_merchants: [&'a [u8]; 10],
    pub customer_known_merchants_len: usize,
    pub merchant_id: &'a [u8],
    pub merchant_mcc: u16,
    pub merchant_avg_amount: f32,
    pub is_online: bool,
    pub card_present: bool,
    pub km_from_home: f32,
    pub last_tx_timestamp: Option<&'a [u8]>,
    pub last_tx_km: Option<f32>,
}

pub fn parse_json_payload(body: &[u8]) -> Option<ParsedTransaction<'_>> {
    let mut id = &b""[..];
    let mut amount = 0.0;
    let mut installments = 0;
    let mut requested_at = &b""[..];
    let mut customer_avg_amount = 0.0;
    let mut customer_tx_count_24h = 0;
    let mut customer_known_merchants = [&b""[..]; 10];
    let mut customer_known_merchants_len = 0;
    let mut merchant_id = &b""[..];
    let mut merchant_mcc = 0;
    let mut merchant_avg_amount = 0.0;
    let mut is_online = false;
    let mut card_present = false;
    let mut km_from_home = 0.0;
    let mut last_tx_timestamp = None;
    let mut last_tx_km = None;
    let mut has_last_tx = false;

    let mut i = 0;
    
    // We expect the top level keys: "id", "transaction", "customer", "merchant", "terminal", "last_transaction"
    while i < body.len() {
        let key = if let Some(k) = next_key(body, &mut i) { k } else { break; };
        match key {
            b"id" => id = parse_str(body, &mut i),
            b"transaction" => {
                skip_to(body, &mut i, b'{')?;
                while let Some(k) = next_key_in_object(body, &mut i) {
                    match k {
                        b"amount" => amount = parse_f32(body, &mut i),
                        b"installments" => installments = parse_u32(body, &mut i) as u8,
                        b"requested_at" => requested_at = parse_str(body, &mut i),
                        _ => skip_value(body, &mut i),
                    }
                }
            }
            b"customer" => {
                skip_to(body, &mut i, b'{')?;
                while let Some(k) = next_key_in_object(body, &mut i) {
                    match k {
                        b"avg_amount" => customer_avg_amount = parse_f32(body, &mut i),
                        b"tx_count_24h" => customer_tx_count_24h = parse_u32(body, &mut i),
                        b"known_merchants" => {
                            skip_to(body, &mut i, b'[')?;
                            while let Some(s) = next_str_in_array(body, &mut i) {
                                if customer_known_merchants_len < 10 {
                                    customer_known_merchants[customer_known_merchants_len] = s;
                                    customer_known_merchants_len += 1;
                                }
                            }
                        }
                        _ => skip_value(body, &mut i),
                    }
                }
            }
            b"merchant" => {
                skip_to(body, &mut i, b'{')?;
                while let Some(k) = next_key_in_object(body, &mut i) {
                    match k {
                        b"id" => merchant_id = parse_str(body, &mut i),
                        b"mcc" => merchant_mcc = parse_u16_from_str(body, &mut i),
                        b"avg_amount" => merchant_avg_amount = parse_f32(body, &mut i),
                        _ => skip_value(body, &mut i),
                    }
                }
            }
            b"terminal" => {
                skip_to(body, &mut i, b'{')?;
                while let Some(k) = next_key_in_object(body, &mut i) {
                    match k {
                        b"is_online" => is_online = parse_bool(body, &mut i),
                        b"card_present" => card_present = parse_bool(body, &mut i),
                        b"km_from_home" => km_from_home = parse_f32(body, &mut i),
                        _ => skip_value(body, &mut i),
                    }
                }
            }
            b"last_transaction" => {
                while i < body.len() && body[i].is_ascii_whitespace() { i += 1; }
                if i + 4 <= body.len() && &body[i..i+4] == b"null" {
                    has_last_tx = false;
                    i += 4;
                } else {
                    has_last_tx = true;
                    skip_to(body, &mut i, b'{')?;
                    while let Some(k) = next_key_in_object(body, &mut i) {
                        match k {
                            b"timestamp" => last_tx_timestamp = Some(parse_str(body, &mut i)),
                            b"km_from_current" => last_tx_km = Some(parse_f32(body, &mut i)),
                            _ => skip_value(body, &mut i),
                        }
                    }
                }
            }
            _ => skip_value(body, &mut i),
        }
    }

    Some(ParsedTransaction {
        id,
        amount,
        installments,
        requested_at,
        customer_avg_amount,
        customer_tx_count_24h,
        customer_known_merchants,
        customer_known_merchants_len,
        merchant_id,
        merchant_mcc,
        merchant_avg_amount,
        is_online,
        card_present,
        km_from_home,
        last_tx_timestamp: if has_last_tx { last_tx_timestamp } else { None },
        last_tx_km: if has_last_tx { last_tx_km } else { None },
    })
}

fn next_key<'a>(body: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    *i += memchr(b'"', &body[*i..])?;
    *i += 1;
    let start = *i;
    *i += memchr(b'"', &body[*i..])?;
    let key = &body[start..*i];
    *i += 1;
    *i += memchr(b':', &body[*i..])?;
    *i += 1;
    while *i < body.len() && body[*i].is_ascii_whitespace() { *i += 1; }
    Some(key)
}

fn next_key_in_object<'a>(body: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    if let Some(pos) = memchr2(b'"', b'}', &body[*i..]) {
        *i += pos;
        if body[*i] == b'}' {
            *i += 1;
            return None;
        }
        next_key(body, i)
    } else {
        *i = body.len();
        None
    }
}

fn next_str_in_array<'a>(body: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    if let Some(pos) = memchr2(b'"', b']', &body[*i..]) {
        *i += pos;
        if body[*i] == b']' {
            *i += 1;
            return None;
        }
        *i += 1;
        let start = *i;
        if let Some(end_pos) = memchr(b'"', &body[*i..]) {
            *i += end_pos;
            let s = &body[start..*i];
            *i += 1;
            return Some(s);
        }
    }
    *i = body.len();
    None
}

fn skip_to(body: &[u8], i: &mut usize, target: u8) -> Option<()> {
    if let Some(pos) = memchr(target, &body[*i..]) {
        *i += pos + 1;
        Some(())
    } else {
        *i = body.len();
        None
    }
}

fn skip_value(body: &[u8], i: &mut usize) {
    while *i < body.len() && body[*i].is_ascii_whitespace() { *i += 1; }
    if *i >= body.len() { return; }
    match body[*i] {
        b'"' => { parse_str(body, i); }
        b'{' => {
            let mut depth = 1;
            *i += 1;
            while *i < body.len() && depth > 0 {
                if let Some(pos) = memchr2(b'{', b'}', &body[*i..]) {
                    *i += pos;
                    if body[*i] == b'{' { depth += 1; } else { depth -= 1; }
                    *i += 1;
                } else {
                    *i = body.len();
                    break;
                }
            }
        }
        b'[' => {
            let mut depth = 1;
            *i += 1;
            while *i < body.len() && depth > 0 {
                if let Some(pos) = memchr2(b'[', b']', &body[*i..]) {
                    *i += pos;
                    if body[*i] == b'[' { depth += 1; } else { depth -= 1; }
                    *i += 1;
                } else {
                    *i = body.len();
                    break;
                }
            }
        }
        _ => {
            if let Some(pos) = memchr3(b',', b'}', b']', &body[*i..]) {
                *i += pos;
            } else {
                *i = body.len();
            }
        }
    }
}

fn parse_f32(body: &[u8], i: &mut usize) -> f32 {
    while *i < body.len() && !body[*i].is_ascii_digit() && body[*i] != b'-' { *i += 1; }
    let mut neg = false;
    if *i < body.len() && body[*i] == b'-' {
        neg = true;
        *i += 1;
    }
    let mut val = 0.0;
    while *i < body.len() && body[*i].is_ascii_digit() {
        val = val * 10.0 + (body[*i] - b'0') as f32;
        *i += 1;
    }
    if *i < body.len() && body[*i] == b'.' {
        *i += 1;
        let mut frac = 0.0;
        let mut div = 10.0;
        while *i < body.len() && body[*i].is_ascii_digit() {
            frac += (body[*i] - b'0') as f32 / div;
            div *= 10.0;
            *i += 1;
        }
        val += frac;
    }
    if neg { -val } else { val }
}

fn parse_u32(body: &[u8], i: &mut usize) -> u32 {
    while *i < body.len() && !body[*i].is_ascii_digit() { *i += 1; }
    let mut val = 0;
    while *i < body.len() && body[*i].is_ascii_digit() {
        val = val * 10 + (body[*i] - b'0') as u32;
        *i += 1;
    }
    val
}

fn parse_str<'a>(body: &'a [u8], i: &mut usize) -> &'a [u8] {
    if let Some(pos) = memchr(b'"', &body[*i..]) {
        *i += pos + 1;
        let start = *i;
        if let Some(end_pos) = memchr(b'"', &body[*i..]) {
            *i += end_pos;
            let s = &body[start..*i];
            *i += 1;
            return s;
        }
    }
    *i = body.len();
    &b""[..]
}

fn parse_bool(body: &[u8], i: &mut usize) -> bool {
    while *i < body.len() && body[*i] != b't' && body[*i] != b'f' { *i += 1; }
    if *i >= body.len() { return false; }
    let b = body[*i] == b't';
    while *i < body.len() && body[*i].is_ascii_alphabetic() { *i += 1; }
    b
}

fn parse_u16_from_str(body: &[u8], i: &mut usize) -> u16 {
    let s = parse_str(body, i);
    let mut val = 0u16;
    for &b in s {
        if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as u16;
        }
    }
    val
}

pub fn parse_timestamp(ts: &[u8]) -> Option<(u8, u8)> {
    if ts.len() < 13 { return None; }
    let hour = (ts[11] - b'0') * 10 + (ts[12] - b'0');
    let year = ((ts[0] - b'0') as i32) * 1000 + ((ts[1] - b'0') as i32) * 100 + ((ts[2] - b'0') as i32) * 10 + ((ts[3] - b'0') as i32);
    let month = ((ts[5] - b'0') as i32) * 10 + ((ts[6] - b'0') as i32);
    let day = ((ts[8] - b'0') as i32) * 10 + ((ts[9] - b'0') as i32);

    if month < 1 || month > 12 { return None; }
    let t = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year;
    if month < 3 { y -= 1; }
    let dow_sun_start = (y + y / 4 - y / 100 + y / 400 + t[(month - 1) as usize] + day) % 7;
    let dow = if dow_sun_start == 0 { 6 } else { (dow_sun_start - 1) as u8 };
    Some((hour, dow))
}

pub fn parse_minutes_diff(ts1: &[u8], ts2: &[u8]) -> Option<f32> {
    fn to_minutes(ts: &[u8]) -> Option<i64> {
        if ts.len() < 16 { return None; }
        let year = ((ts[0] - b'0') as i64) * 1000 + ((ts[1] - b'0') as i64) * 100 + ((ts[2] - b'0') as i64) * 10 + ((ts[3] - b'0') as i64);
        let month = ((ts[5] - b'0') as i64) * 10 + ((ts[6] - b'0') as i64);
        let day = ((ts[8] - b'0') as i64) * 10 + ((ts[9] - b'0') as i64);
        let hour = ((ts[11] - b'0') as i64) * 10 + ((ts[12] - b'0') as i64);
        let min = ((ts[14] - b'0') as i64) * 10 + ((ts[15] - b'0') as i64);

        let mut total_days = (year - 2000) * 365 + (year - 2000) / 4;
        let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        for i in 0..(month - 1) as usize { total_days += month_days[i]; }
        if month > 2 && year % 4 == 0 { total_days += 1; }
        total_days += day;
        Some(total_days * 1440 + hour * 60 + min)
    }
    let m1 = to_minutes(ts1)?;
    let m2 = to_minutes(ts2)?;
    Some((m1 - m2).abs() as f32)
}


