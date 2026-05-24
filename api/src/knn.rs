use crate::mmap::{IvfData, Record};
use crate::logging::{Level, Category};
use std::arch::x86_64::*;

const K: usize = 7;
const N_L1: usize = 256;
const N_L2_PER_L1: usize = 256;
const N_PROBE_L1: usize = 16;
const N_PROBE_L2: usize = 256;
const N_PROBE_L2_EXTENDED: usize = 512;
const SCORE_EPS: f32 = 1e-6;
const APPROVAL_THRESHOLD: f32 = 0.44;
const CONFIDENCE_THRESHOLD_LOW: f32 = 0.38;
const CONFIDENCE_THRESHOLD_HIGH: f32 = 0.50;

// Feature weights for the 14 features (0-13). 14 and 15 are metadata.
static FEATURE_WEIGHTS: [f32; 16] = [
    1.0038165, 0.665417, 0.8668326, 0.5379362, 0.5, 0.3, 0.3701757, 1.0, 1.2, 1.2648705, 0.81239825, 1.051987, 0.8247206, 2.0315619, 0.0, 0.0,
];

pub struct IvfIndex {
    data: IvfData,
}

impl IvfIndex {
    pub fn new(data: IvfData) -> Self {
        Self { data }
    }

    pub fn search(&self, query: &[f32; 16], records: &[Record]) -> (bool, f32) {
        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("avx2") {
                return unsafe { self.search_hivf_avx2(query, records) };
            }
        }
        
        self.search_scalar(query, records)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn search_hivf_avx2(&self, query: &[f32; 16], records: &[Record]) -> (bool, f32) {
        let l1_centroids = self.data.l1_centroids;
        let l2_centroids = self.data.l2_centroids;
        let offsets = self.data.offsets;

        let q_low = _mm256_loadu_ps(query.as_ptr());
        let q_high = _mm256_loadu_ps(query.as_ptr().add(8));
        let w_low = _mm256_loadu_ps(FEATURE_WEIGHTS.as_ptr());
        let w_high = _mm256_loadu_ps(FEATURE_WEIGHTS.as_ptr().add(8));
        let abs_mask = _mm256_castsi256_ps(_mm256_set1_epi32(0x7fffffff));

        // 1. Scan L1 Root
        let mut dists_l1 = [(0.0f32, 0usize); N_L1];
        let mut i = 0;
        while i + 3 < N_L1 {
            let (d0, d1, d2, d3) = self.dist_avx2_weighted_x4(
                q_low, q_high, w_low, w_high, abs_mask,
                &l1_centroids[i], &l1_centroids[i+1], &l1_centroids[i+2], &l1_centroids[i+3]
            );
            dists_l1[i] = (d0, i);
            dists_l1[i+1] = (d1, i+1);
            dists_l1[i+2] = (d2, i+2);
            dists_l1[i+3] = (d3, i+3);
            i += 4;
        }

        let (best_l1, _, _) = dists_l1.select_nth_unstable_by(N_PROBE_L1, |a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // 2. Scan L2 Leaves for selected L1s
        let mut dists_l2 = [(0.0f32, 0usize); N_PROBE_L1 * N_L2_PER_L1];
        let mut l2_count = 0;
        for probe_idx in 0..N_PROBE_L1 {
            let l1_idx = best_l1[probe_idx].1;
            let l2_base = l1_idx * N_L2_PER_L1;
            let mut j = 0;
            while j + 7 < N_L2_PER_L1 {
                let (d0, d1, d2, d3) = self.dist_avx2_weighted_x4(
                    q_low, q_high, w_low, w_high, abs_mask,
                    &l2_centroids[l2_base + j], &l2_centroids[l2_base + j + 1], 
                    &l2_centroids[l2_base + j + 2], &l2_centroids[l2_base + j + 3]
                );
                let (d4, d5, d6, d7) = self.dist_avx2_weighted_x4(
                    q_low, q_high, w_low, w_high, abs_mask,
                    &l2_centroids[l2_base + j + 4], &l2_centroids[l2_base + j + 5], 
                    &l2_centroids[l2_base + j + 6], &l2_centroids[l2_base + j + 7]
                );

                dists_l2[l2_count] = (d0, l2_base + j);
                dists_l2[l2_count+1] = (d1, l2_base + j + 1);
                dists_l2[l2_count+2] = (d2, l2_base + j + 2);
                dists_l2[l2_count+3] = (d3, l2_base + j + 3);
                dists_l2[l2_count+4] = (d4, l2_base + j + 4);
                dists_l2[l2_count+5] = (d5, l2_base + j + 5);
                dists_l2[l2_count+6] = (d6, l2_base + j + 6);
                dists_l2[l2_count+7] = (d7, l2_base + j + 7);
                
                j += 8;
                l2_count += 8;
            }
        }

        let (best_l2_unsorted, _, _) = dists_l2.select_nth_unstable_by(N_PROBE_L2_EXTENDED, |a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let (best_l2_first_256, _, _) = best_l2_unsorted.select_nth_unstable_by(N_PROBE_L2, |a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        best_l2_first_256.sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let best_l2 = best_l2_unsorted;

        // 3. Scan Records in top 256 clusters
        let mut top_k = [(f32::MAX, 0u8, 0usize); K];
        let mut max_dist = f32::MAX;

        for probe_idx in 0..N_PROBE_L2 {
            max_dist = self.scan_cluster_weighted(best_l2[probe_idx].1, records, offsets, q_low, q_high, w_low, w_high, abs_mask, &mut top_k, max_dist);
            if top_k[0].0 < 0.05 {
                let label = top_k[0].1;
                return (label == 0, label as f32);
            }
        }

        let (approved, score) = self.calculate_fraud_score_weighted(top_k);

        // 4. Adaptive Probing
        if (score > CONFIDENCE_THRESHOLD_LOW && score < CONFIDENCE_THRESHOLD_HIGH) || top_k[0].0 > 1.5 {
            let mut extended_top_k = top_k;
            let mut extended_max_dist = max_dist;
            for probe_idx in N_PROBE_L2..N_PROBE_L2_EXTENDED {
                extended_max_dist = self.scan_cluster_weighted(best_l2[probe_idx].1, records, offsets, q_low, q_high, w_low, w_high, abs_mask, &mut extended_top_k, extended_max_dist);
                if extended_top_k[0].0 < 0.05 {
                    let label = extended_top_k[0].1;
                    return (label == 0, label as f32);
                }
            }
            return self.calculate_fraud_score_weighted(extended_top_k);
        }

        (approved, score)
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn scan_cluster_weighted(
        &self,
        l2_idx: usize,
        records: &[Record],
        offsets: &[u32],
        q_low: __m256,
        q_high: __m256,
        w_low: __m256,
        w_high: __m256,
        abs_mask: __m256,
        top_k: &mut [(f32, u8, usize); K],
        mut max_dist: f32
    ) -> f32 {
        let start = offsets[l2_idx] as usize;
        let end = offsets[l2_idx + 1] as usize;
        let cluster_records = &records[start..end];

        let mut k = 0;
        while k + 3 < cluster_records.len() {
            if k + 12 < cluster_records.len() {
                _mm_prefetch(cluster_records[k + 12].vector.as_ptr() as *const i8, _MM_HINT_T0);
            }
            let (d0, d1, d2, d3) = self.dist_avx2_weighted_x4(
                q_low, q_high, w_low, w_high, abs_mask,
                &cluster_records[k].vector, &cluster_records[k+1].vector, 
                &cluster_records[k+2].vector, &cluster_records[k+3].vector
            );

            if d0 < max_dist { max_dist = self.update_top_k_packed(top_k, d0, cluster_records[k].vector[15]); }
            if d1 < max_dist { max_dist = self.update_top_k_packed(top_k, d1, cluster_records[k+1].vector[15]); }
            if d2 < max_dist { max_dist = self.update_top_k_packed(top_k, d2, cluster_records[k+2].vector[15]); }
            if d3 < max_dist { max_dist = self.update_top_k_packed(top_k, d3, cluster_records[k+3].vector[15]); }
            k += 4;
        }
        while k < cluster_records.len() {
            let dist = self.dist_avx2_weighted(q_low, q_high, w_low, w_high, abs_mask, &cluster_records[k].vector);
            if dist < max_dist {
                max_dist = self.update_top_k_packed(top_k, dist, cluster_records[k].vector[15]);
            }
            k += 1;
        }
        max_dist
    }

    #[inline(always)]
    fn update_top_k_packed(&self, top_k: &mut [(f32, u8, usize); K], dist: f32, packed_f32: f32) -> f32 {
        let packed = packed_f32.to_bits();
        let label = (packed & 1) as u8;
        let id = (packed >> 1) as usize;

        if dist < top_k[K - 1].0 {
            let mut pos = K - 1;
            while pos > 0 && dist < top_k[pos - 1].0 {
                top_k[pos] = top_k[pos - 1];
                pos -= 1;
            }
            top_k[pos] = (dist, label, id);
        }
        top_k[K - 1].0
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn dist_avx2_weighted(&self, q_low: __m256, q_high: __m256, w_low: __m256, w_high: __m256, abs_mask: __m256, v2: &[f32; 16]) -> f32 {
        let b_low = _mm256_loadu_ps(v2.as_ptr());
        let b_high = _mm256_loadu_ps(v2.as_ptr().add(8));
        let diff_low = _mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b_low), abs_mask), w_low);
        let diff_high = _mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b_high), abs_mask), w_high);
        let sum_vec = _mm256_add_ps(diff_low, diff_high);
        self.hsum_avx2(sum_vec)
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn dist_avx2_weighted_x4(&self, q_low: __m256, q_high: __m256, w_low: __m256, w_high: __m256, abs_mask: __m256, v0: &[f32; 16], v1: &[f32; 16], v2: &[f32; 16], v3: &[f32; 16]) -> (f32, f32, f32, f32) {
        let b0_low = _mm256_loadu_ps(v0.as_ptr());
        let b1_low = _mm256_loadu_ps(v1.as_ptr());
        let b2_low = _mm256_loadu_ps(v2.as_ptr());
        let b3_low = _mm256_loadu_ps(v3.as_ptr());
        let b0_high = _mm256_loadu_ps(v0.as_ptr().add(8));
        let b1_high = _mm256_loadu_ps(v1.as_ptr().add(8));
        let b2_high = _mm256_loadu_ps(v2.as_ptr().add(8));
        let b3_high = _mm256_loadu_ps(v3.as_ptr().add(8));
        
        let s0 = _mm256_add_ps(_mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b0_low), abs_mask), w_low), _mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b0_high), abs_mask), w_high));
        let s1 = _mm256_add_ps(_mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b1_low), abs_mask), w_low), _mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b1_high), abs_mask), w_high));
        let s2 = _mm256_add_ps(_mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b2_low), abs_mask), w_low), _mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b2_high), abs_mask), w_high));
        let s3 = _mm256_add_ps(_mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b3_low), abs_mask), w_low), _mm256_mul_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b3_high), abs_mask), w_high));
        
        (self.hsum_avx2(s0), self.hsum_avx2(s1), self.hsum_avx2(s2), self.hsum_avx2(s3))
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn hsum_avx2(&self, sum_vec: __m256) -> f32 {
        let x128_low = _mm256_castps256_ps128(sum_vec);
        let x128_high = _mm256_extractf128_ps(sum_vec, 1);
        let x_sum = _mm_add_ps(x128_low, x128_high);
        let x_shuf = _mm_movehdup_ps(x_sum);
        let x_sum2 = _mm_add_ps(x_sum, x_shuf);
        let x_shuf2 = _mm_movehl_ps(x_sum2, x_sum2);
        let x_final = _mm_add_ss(x_sum2, x_shuf2);
        _mm_cvtss_f32(x_final)
    }

    fn search_scalar(&self, query: &[f32; 16], records: &[Record]) -> (bool, f32) {
        let mut dists_l1 = [(0.0f32, 0usize); N_L1];
        for i in 0..N_L1 {
            let dist = manhattan_distance_scalar(query, &self.data.l1_centroids[i]);
            dists_l1[i] = (dist, i);
        }

        let (best_l1, _, _) = dists_l1.select_nth_unstable_by(N_PROBE_L1, |a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut dists_l2 = [(0.0f32, 0usize); N_PROBE_L1 * N_L2_PER_L1];
        let mut l2_count = 0;
        for probe_idx in 0..N_PROBE_L1 {
            let l1_idx = best_l1[probe_idx].1;
            let l2_base = l1_idx * N_L2_PER_L1;
            for j in 0..N_L2_PER_L1 {
                let dist = manhattan_distance_scalar(query, &self.data.l2_centroids[l2_base + j]);
                dists_l2[l2_count] = (dist, l2_base + j);
                l2_count += 1;
            }
        }

        let (best_l2_unsorted, _, _) = dists_l2.select_nth_unstable_by(N_PROBE_L2_EXTENDED, |a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let (best_l2_first_256, _, _) = best_l2_unsorted.select_nth_unstable_by(N_PROBE_L2, |a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        best_l2_first_256.sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let best_l2 = best_l2_unsorted;

        let mut top_k = [(f32::MAX, 0u8, 0usize); K];
        let mut max_dist = f32::MAX;
        for probe_idx in 0..N_PROBE_L2 {
            let l2_idx = best_l2[probe_idx].1;
            let start = self.data.offsets[l2_idx] as usize;
            let end = self.data.offsets[l2_idx + 1] as usize;
            for record in &records[start..end] {
                let dist = manhattan_distance_scalar(query, &record.vector);
                if dist < max_dist {
                    max_dist = self.update_top_k_packed_scalar(&mut top_k, dist, record.vector[15]);
                }
            }
            if top_k[0].0 < 0.05 {
                let label = top_k[0].1;
                return (label == 0, label as f32);
            }
        }
        
        let (_, score) = self.calculate_fraud_score_weighted(top_k);
        if (score > CONFIDENCE_THRESHOLD_LOW && score < CONFIDENCE_THRESHOLD_HIGH) || top_k[0].0 > 1.5 {
            for probe_idx in N_PROBE_L2..N_PROBE_L2_EXTENDED {
                let l2_idx = best_l2[probe_idx].1;
                let start = self.data.offsets[l2_idx] as usize;
                let end = self.data.offsets[l2_idx + 1] as usize;
                for record in &records[start..end] {
                    let dist = manhattan_distance_scalar(query, &record.vector);
                    if dist < max_dist {
                        max_dist = self.update_top_k_packed_scalar(&mut top_k, dist, record.vector[15]);
                    }
                }
                if top_k[0].0 < 0.05 {
                    let label = top_k[0].1;
                    return (label == 0, label as f32);
                }
            }
        }

        self.calculate_fraud_score_weighted(top_k)
    }

    #[inline(always)]
    fn update_top_k_packed_scalar(&self, top_k: &mut [(f32, u8, usize); K], dist: f32, packed_f32: f32) -> f32 {
        let packed = packed_f32.to_bits();
        let label = (packed & 1) as u8;
        let id = (packed >> 1) as usize;

        if dist < top_k[K - 1].0 {
            let mut pos = K - 1;
            while pos > 0 && dist < top_k[pos - 1].0 {
                top_k[pos] = top_k[pos - 1];
                pos -= 1;
            }
            top_k[pos] = (dist, label, id);
        }
        top_k[K - 1].0
    }

    fn calculate_fraud_score_weighted(&self, top_k: [(f32, u8, usize); K]) -> (bool, f32) {
        if top_k[0].0 < 0.05 {
            let label = top_k[0].1;
            return (label == 0, label as f32);
        }
        let mut fraud_weight = 0.0f32;
        let mut total_weight = 0.0f32;
        for &(dist, label, _) in &top_k {
            if dist == f32::MAX { continue; }
            // Kernel: exp(-dist * 0.5)
            let weight = (-dist * 0.5).exp();
            total_weight += weight;
            fraud_weight += weight * label as f32;
        }
        let fraud_score = if total_weight > 0.0 { fraud_weight / total_weight } else { 0.0 };
        (fraud_score < APPROVAL_THRESHOLD, fraud_score)
    }
}

#[inline(always)]
fn manhattan_distance_scalar(v1: &[f32; 16], v2: &[f32; 16]) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 { 
        sum += (v1[i] - v2[i]).abs() * FEATURE_WEIGHTS[i]; 
    }
    sum
}
