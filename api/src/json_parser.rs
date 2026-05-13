pub struct ParsedTransaction<'a> {
    pub amount: f32,
    pub installments: u8,
    pub requested_at: &'a [u8],
    pub customer_avg_amount: f32,
    pub customer_tx_count_24h: u32,
    pub customer_known_merchants: Vec<&'a [u8]>,
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
    let mut i = 0;

    macro_rules! skip_to_colon {
        () => {
            while i < body.len() && body[i] != b':' {
                i += 1;
            }
            if i >= body.len() {
                return None;
            }
            i += 1; // skip ':'
        };
    }

    macro_rules! parse_f32 {
        () => {{
            while i < body.len() && !body[i].is_ascii_digit() && body[i] != b'-' {
                i += 1;
            }
            let start = i;
            while i < body.len() && (body[i].is_ascii_digit() || body[i] == b'.' || body[i] == b'-')
            {
                i += 1;
            }
            std::str::from_utf8(&body[start..i])
                .ok()?
                .parse::<f32>()
                .ok()?
        }};
    }

    macro_rules! parse_u32 {
        () => {{
            while i < body.len() && !body[i].is_ascii_digit() {
                i += 1;
            }
            let start = i;
            while i < body.len() && body[i].is_ascii_digit() {
                i += 1;
            }
            std::str::from_utf8(&body[start..i])
                .ok()?
                .parse::<u32>()
                .ok()?
        }};
    }

    macro_rules! parse_str {
        () => {{
            while i < body.len() && body[i] != b'"' {
                i += 1;
            }
            i += 1;
            let start = i;
            while i < body.len() && body[i] != b'"' {
                i += 1;
            }
            let s = &body[start..i];
            i += 1;
            s
        }};
    }

    macro_rules! parse_bool {
        () => {{
            while i < body.len() && body[i] != b't' && body[i] != b'f' {
                i += 1;
            }
            if i >= body.len() { return None; }
            let b = body[i] == b't';
            while i < body.len() && body[i].is_ascii_alphabetic() {
                i += 1;
            }
            b
        }};
    }

    skip_to_colon!(); // id
    let _id = parse_str!();

    skip_to_colon!(); // transaction (object start)
    skip_to_colon!(); // amount
    let amount = parse_f32!();

    skip_to_colon!(); // installments
    let installments = parse_u32!() as u8;

    skip_to_colon!(); // requested_at
    let requested_at = parse_str!();

    skip_to_colon!(); // customer (object start)
    skip_to_colon!(); // avg_amount
    let customer_avg_amount = parse_f32!();

    skip_to_colon!(); // tx_count_24h
    let customer_tx_count_24h = parse_u32!();

    skip_to_colon!(); // known_merchants (array start)
    let mut customer_known_merchants = Vec::with_capacity(8);
    while i < body.len() && body[i] != b']' {
        if body[i] == b'"' {
            customer_known_merchants.push(parse_str!());
        } else {
            i += 1;
        }
    }

    skip_to_colon!(); // merchant (object start)
    skip_to_colon!(); // id
    let merchant_id = parse_str!();

    skip_to_colon!(); // mcc
    let merchant_mcc = parse_str!();
    let merchant_mcc_u16 = std::str::from_utf8(merchant_mcc)
        .ok()?
        .parse::<u16>()
        .ok()?;

    skip_to_colon!(); // avg_amount
    let merchant_avg_amount = parse_f32!();

    skip_to_colon!(); // terminal (object start)
    skip_to_colon!(); // is_online
    let is_online = parse_bool!();

    skip_to_colon!(); // card_present
    let card_present = parse_bool!();

    skip_to_colon!(); // km_from_home
    let km_from_home = parse_f32!();

    skip_to_colon!(); // last_transaction
    while i < body.len() && body[i].is_ascii_whitespace() {
        i += 1;
    }
    if i + 4 <= body.len() && &body[i..i + 4] == b"null" {
        return Some(ParsedTransaction {
            amount,
            installments,
            requested_at,
            customer_avg_amount,
            customer_tx_count_24h,
            customer_known_merchants,
            merchant_id,
            merchant_mcc: merchant_mcc_u16,
            merchant_avg_amount,
            is_online,
            card_present,
            km_from_home,
            last_tx_timestamp: None,
            last_tx_km: None,
        });
    }

    if i >= body.len() { return None; }

    skip_to_colon!(); // timestamp
    let last_tx_timestamp = Some(parse_str!());

    skip_to_colon!(); // km_from_current
    let last_tx_km = Some(parse_f32!());

    Some(ParsedTransaction {
        amount,
        installments,
        requested_at,
        customer_avg_amount,
        customer_tx_count_24h,
        customer_known_merchants,
        merchant_id,
        merchant_mcc: merchant_mcc_u16,
        merchant_avg_amount,
        is_online,
        card_present,
        km_from_home,
        last_tx_timestamp,
        last_tx_km,
    })
}

pub fn parse_timestamp(ts: &[u8]) -> Option<(u8, u8)> {
    if ts.len() < 13 {
        return None;
    }
    let hour = std::str::from_utf8(&ts[11..13]).ok()?.parse::<u8>().ok()?;
    let year = std::str::from_utf8(&ts[0..4]).ok()?.parse::<i32>().ok()?;
    let month = std::str::from_utf8(&ts[5..7]).ok()?.parse::<i32>().ok()?;
    let day = std::str::from_utf8(&ts[8..10]).ok()?.parse::<i32>().ok()?;

    if month < 1 || month > 12 {
        return None;
    }

    let t = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year;
    if month < 3 {
        y -= 1;
    }
    let dow_sun_start = (y + y / 4 - y / 100 + y / 400 + t[(month - 1) as usize] + day) % 7;
    let dow = if dow_sun_start == 0 {
        6
    } else {
        (dow_sun_start - 1) as u8
    };
    Some((hour, dow))
}

pub fn parse_minutes_diff(ts1: &[u8], ts2: &[u8]) -> Option<f32> {
    fn to_minutes(ts: &[u8]) -> Option<i64> {
        if ts.len() < 16 {
            return None;
        }
        let year = std::str::from_utf8(&ts[0..4]).ok()?.parse::<i64>().ok()?;
        let month = std::str::from_utf8(&ts[5..7]).ok()?.parse::<i64>().ok()?;
        let day = std::str::from_utf8(&ts[8..10]).ok()?.parse::<i64>().ok()?;
        let hour = std::str::from_utf8(&ts[11..13]).ok()?.parse::<i64>().ok()?;
        let min = std::str::from_utf8(&ts[14..16]).ok()?.parse::<i64>().ok()?;

        let mut total_days = (year - 2000) * 365 + (year - 2000) / 4;
        let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        for i in 0..(month - 1) as usize {
            total_days += month_days[i];
        }
        if month > 2 && year % 4 == 0 {
            total_days += 1;
        }
        total_days += day;
        Some(total_days * 1440 + hour * 60 + min)
    }

    let m1 = to_minutes(ts1)?;
    let m2 = to_minutes(ts2)?;
    Some((m1 - m2).abs() as f32)
}
