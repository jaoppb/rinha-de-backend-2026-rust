use crate::mmap::{IvfData, Record};
use std::arch::x86_64::*;

const K: usize = 7;
const N_CENTROIDS: usize = 4096;
const N_PROBE: usize = 8; // Optimized for <1ms target
const SCORE_EPS: f32 = 1e-6;
const APPROVAL_THRESHOLD: f32 = 0.44;

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
                return unsafe { self.search_avx2(query, records) };
            }
        }
        
        self.search_scalar(query, records)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn search_avx2(&self, query: &[f32; 16], records: &[Record]) -> (bool, f32) {
        let centroids = self.data.centroids;
        let offsets = self.data.offsets;

        let q_low = _mm256_loadu_ps(query.as_ptr());
        let q_high = _mm256_loadu_ps(query.as_ptr().add(8));
        let abs_mask = _mm256_castsi256_ps(_mm256_set1_epi32(0x7fffffff));
        let mask_high = _mm256_castsi256_ps(_mm256_set_epi32(0, 0, -1, -1, -1, -1, -1, -1));

        let mut best_probes = [(f32::MAX, 0usize); N_PROBE];
        
        let mut i = 0;
        while i + 3 < N_CENTROIDS {
            let (d0, d1, d2, d3) = self.dist_avx2_preloaded_x4(
                q_low, q_high, abs_mask, mask_high,
                &centroids[i], &centroids[i+1], &centroids[i+2], &centroids[i+3]
            );

            self.update_best_probes(&mut best_probes, d0, i);
            self.update_best_probes(&mut best_probes, d1, i + 1);
            self.update_best_probes(&mut best_probes, d2, i + 2);
            self.update_best_probes(&mut best_probes, d3, i + 3);
            
            i += 4;
        }

        let mut top_k = [(f32::MAX, 0u8); K];
        let mut max_dist = f32::MAX;

        for i in 0..N_PROBE {
            let (_, c_idx) = best_probes[i];
            let start = offsets[c_idx] as usize;
            let end = offsets[c_idx + 1] as usize;
            let cluster_records = &records[start..end];

            let mut k = 0;
            while k + 3 < cluster_records.len() {
                if k + 12 < cluster_records.len() {
                    _mm_prefetch(cluster_records[k + 12].vector.as_ptr() as *const i8, _MM_HINT_T0);
                }
                let (d0, d1, d2, d3) = self.dist_avx2_preloaded_x4(
                    q_low, q_high, abs_mask, mask_high,
                    &cluster_records[k].vector, &cluster_records[k+1].vector, &cluster_records[k+2].vector, &cluster_records[k+3].vector
                );
                if d0 < max_dist { max_dist = update_top_k(&mut top_k, d0, cluster_records[k].vector[15] as u8); }
                if d1 < max_dist { max_dist = update_top_k(&mut top_k, d1, cluster_records[k+1].vector[15] as u8); }
                if d2 < max_dist { max_dist = update_top_k(&mut top_k, d2, cluster_records[k+2].vector[15] as u8); }
                if d3 < max_dist { max_dist = update_top_k(&mut top_k, d3, cluster_records[k+3].vector[15] as u8); }
                k += 4;
            }
            while k < cluster_records.len() {
                let dist = self.dist_avx2_preloaded(q_low, q_high, abs_mask, mask_high, &cluster_records[k].vector);
                if dist < max_dist {
                    max_dist = update_top_k(&mut top_k, dist, cluster_records[k].vector[15] as u8);
                }
                k += 1;
            }
        }

        self.calculate_fraud_score(top_k)
    }

    #[inline(always)]
    fn update_best_probes(&self, best_probes: &mut [(f32, usize); N_PROBE], dist: f32, idx: usize) {
        if dist < best_probes[N_PROBE - 1].0 {
            let mut pos = N_PROBE - 1;
            while pos > 0 && dist < best_probes[pos - 1].0 {
                best_probes[pos] = best_probes[pos - 1];
                pos -= 1;
            }
            best_probes[pos] = (dist, idx);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn dist_avx2_preloaded(&self, q_low: __m256, q_high: __m256, abs_mask: __m256, mask_high: __m256, v2: &[f32; 16]) -> f32 {
        let b_low = _mm256_loadu_ps(v2.as_ptr());
        let b_high = _mm256_loadu_ps(v2.as_ptr().add(8));
        let abs_low = _mm256_and_ps(_mm256_sub_ps(q_low, b_low), abs_mask);
        let abs_high = _mm256_and_ps(_mm256_sub_ps(q_high, b_high), abs_mask);
        let masked_high = _mm256_and_ps(abs_high, mask_high);
        let sum_vec = _mm256_add_ps(abs_low, masked_high);
        self.hsum_avx2(sum_vec)
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn dist_avx2_preloaded_x4(&self, q_low: __m256, q_high: __m256, abs_mask: __m256, mask_high: __m256, v0: &[f32; 16], v1: &[f32; 16], v2: &[f32; 16], v3: &[f32; 16]) -> (f32, f32, f32, f32) {
        let b0_low = _mm256_loadu_ps(v0.as_ptr());
        let b1_low = _mm256_loadu_ps(v1.as_ptr());
        let b2_low = _mm256_loadu_ps(v2.as_ptr());
        let b3_low = _mm256_loadu_ps(v3.as_ptr());
        let b0_high = _mm256_loadu_ps(v0.as_ptr().add(8));
        let b1_high = _mm256_loadu_ps(v1.as_ptr().add(8));
        let b2_high = _mm256_loadu_ps(v2.as_ptr().add(8));
        let b3_high = _mm256_loadu_ps(v3.as_ptr().add(8));
        let s0 = _mm256_add_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b0_low), abs_mask), _mm256_and_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b0_high), abs_mask), mask_high));
        let s1 = _mm256_add_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b1_low), abs_mask), _mm256_and_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b1_high), abs_mask), mask_high));
        let s2 = _mm256_add_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b2_low), abs_mask), _mm256_and_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b2_high), abs_mask), mask_high));
        let s3 = _mm256_add_ps(_mm256_and_ps(_mm256_sub_ps(q_low, b3_low), abs_mask), _mm256_and_ps(_mm256_and_ps(_mm256_sub_ps(q_high, b3_high), abs_mask), mask_high));
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
        let mut best_probes = [(f32::MAX, 0usize); N_PROBE];
        for i in 0..N_CENTROIDS {
            let mut sum = 0.0;
            for j in 0..14 { sum += (query[j] - self.data.centroids[i][j]).abs(); }
            self.update_best_probes(&mut best_probes, sum, i);
        }
        let mut top_k = [(f32::MAX, 0u8); K];
        let mut max_dist = f32::MAX;
        for i in 0..N_PROBE {
            let (_, c_idx) = best_probes[i];
            let start = self.data.offsets[c_idx] as usize;
            let end = self.data.offsets[c_idx + 1] as usize;
            for record in &records[start..end] {
                let mut sum = 0.0;
                for j in 0..14 {
                    sum += (query[j] - record.vector[j]).abs();
                    if sum >= max_dist { break; }
                }
                if sum < max_dist { max_dist = update_top_k(&mut top_k, sum, record.vector[15] as u8); }
            }
        }
        self.calculate_fraud_score(top_k)
    }

    fn calculate_fraud_score(&self, top_k: [(f32, u8); K]) -> (bool, f32) {
        let mut fraud_weight = 0.0f32;
        let mut total_weight = 0.0f32;
        for &(dist, label) in &top_k {
            if dist == f32::MAX { continue; }
            let weight = 1.0 / (1.0 + dist + SCORE_EPS);
            total_weight += weight;
            fraud_weight += weight * label as f32;
        }
        let fraud_score = if total_weight > 0.0 { fraud_weight / total_weight } else { 0.0 };
        (fraud_score < APPROVAL_THRESHOLD, fraud_score)
    }
}

#[inline(always)]
fn update_top_k(top_k: &mut [(f32, u8); K], dist: f32, label: u8) -> f32 {
    let mut pos = K - 1;
    while pos > 0 && dist < top_k[pos - 1].0 {
        top_k[pos] = top_k[pos - 1];
        pos -= 1;
    }
    top_k[pos] = (dist, label);
    top_k[K - 1].0
}
