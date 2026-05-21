use crate::json_parser::{ParsedTransaction, parse_minutes_diff, parse_timestamp};
use crate::mmap::LookupData;

#[inline(always)]
fn clamp(val: f32) -> f32 {
    if val < 0.0 {
        0.0
    } else if val > 1.0 {
        1.0
    } else {
        val
    }
}

pub fn vectorize(tx: &ParsedTransaction, lookups: &LookupData) -> Option<[f32; 14]> {
    let mut vec = [0.0f32; 14];
    let norm = &lookups.normalization;

    // 0. amount
    vec[0] = clamp(tx.amount / norm.max_amount);

    // 1. installments
    vec[1] = clamp(tx.installments as f32 / norm.max_installments);

    // 2. amount_vs_avg
    vec[2] = clamp((tx.amount / tx.customer_avg_amount) / norm.amount_vs_avg_ratio);

    // 3. hour_of_day & 4. day_of_week
    let (hour, dow) = parse_timestamp(tx.requested_at)?;
    vec[3] = hour as f32 / 23.0;
    vec[4] = dow as f32 / 6.0;

    // 5. minutes_since_last_tx & 6. km_from_last_tx
    if let (Some(last_ts), Some(last_km)) = (tx.last_tx_timestamp, tx.last_tx_km) {
        let minutes = parse_minutes_diff(tx.requested_at, last_ts)?;
        vec[5] = clamp(minutes / norm.max_minutes);
        vec[6] = clamp(last_km / norm.max_km);
    } else {
        vec[5] = -1.0;
        vec[6] = -1.0;
    }

    // 7. km_from_home
    vec[7] = clamp(tx.km_from_home / norm.max_km);

    // 8. tx_count_24h
    vec[8] = clamp(tx.customer_tx_count_24h as f32 / norm.max_tx_count_24h);

    // 9. is_online
    vec[9] = if tx.is_online { 1.0 } else { 0.0 };

    // 10. card_present
    vec[10] = if tx.card_present { 1.0 } else { 0.0 };

    // 11. unknown_merchant
    let mut is_known = false;
    for idx in 0..tx.customer_known_merchants_len {
        if tx.customer_known_merchants[idx] == tx.merchant_id {
            is_known = true;
            break;
        }
    }
    vec[11] = if is_known { 0.0 } else { 1.0 };

    // 12. mcc_risk
    vec[12] = lookups.mcc_risks[tx.merchant_mcc as usize];

    // 13. merchant_avg_amount
    vec[13] = clamp(tx.merchant_avg_amount / norm.max_merchant_avg_amount);

    Some(vec)
}
