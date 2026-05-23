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

pub fn vectorize(tx: &ParsedTransaction, lookups: &LookupData) -> Option<[f32; 16]> {
    let mut vec = [0.0f32; 16];
    let norm = &lookups.normalization;

    // 0. ln(1 + amount)
    vec[0] = (1.0 + tx.amount).ln() / (1.0 + norm.max_amount).ln();

    // 1. installments
    vec[1] = clamp(tx.installments as f32 / norm.max_installments);

    // 2. amount_vs_avg_ratio
    vec[2] = clamp((tx.amount / tx.customer_avg_amount) / norm.amount_vs_avg_ratio);

    // 3. hour_sin & 4. hour_cos
    let (hour, dow) = parse_timestamp(tx.requested_at)?;
    let hour_rad = (hour as f32) * 2.0 * std::f32::consts::PI / 24.0;
    vec[3] = hour_rad.sin();
    vec[4] = hour_rad.cos();

    // 5. day_sin & 6. day_cos
    let day_rad = (dow as f32) * 2.0 * std::f32::consts::PI / 7.0;
    vec[5] = day_rad.sin();
    vec[6] = day_rad.cos();

    // 7. ln(1 + minutes_since_last_tx)
    if let (Some(last_ts), Some(last_km)) = (tx.last_tx_timestamp, tx.last_tx_km) {
        let minutes = parse_minutes_diff(tx.requested_at, last_ts)?;
        vec[7] = (1.0 + minutes).ln() / (1.0 + norm.max_minutes).ln();
        vec[8] = clamp(last_km / norm.max_km);
    } else {
        vec[7] = -1.0;
        vec[8] = -1.0;
    }

    // 9. km_from_home
    vec[9] = clamp(tx.km_from_home / norm.max_km);

    // 10. tx_count_24h
    vec[10] = clamp(tx.customer_tx_count_24h as f32 / norm.max_tx_count_24h);

    // 11. Packed Binary (is_online, card_present, unknown_merchant)
    let mut packed_binary = 0.0f32;
    if tx.is_online { packed_binary += 1.0; }
    if tx.card_present { packed_binary += 2.0; }
    
    let mut is_known = false;
    for idx in 0..tx.customer_known_merchants_len {
        if tx.customer_known_merchants[idx] == tx.merchant_id {
            is_known = true;
            break;
        }
    }
    if !is_known { packed_binary += 4.0; }
    vec[11] = packed_binary / 7.0;

    // 12. mcc_risk
    vec[12] = lookups.mcc_risks[tx.merchant_mcc as usize];

    // 13. merchant_avg_amount
    vec[13] = clamp(tx.merchant_avg_amount / norm.max_merchant_avg_amount);

    // 14. Padding/Spare
    vec[14] = 0.0;

    // 15. Packed ID & Label Placeholder
    vec[15] = 0.0;

    Some(vec)
}

