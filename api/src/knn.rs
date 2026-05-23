use crate::mmap::{IvfData, Record};
use std::arch::x86_64::*;

const K: usize = 7;
const N_L1: usize = 256;
const N_L2_PER_L1: usize = 256;
const N_PROBE_L1: usize = 16;
const N_PROBE_L2: usize = 64;
const N_PROBE_L2_EXTENDED: usize = 256; // Extended probe for adaptive search
const SCORE_EPS: f32 = 1e-6;
const APPROVAL_THRESHOLD: f32 = 0.44;
const CONFIDENCE_THRESHOLD_LOW: f32 = 0.38; // Lower bound for "borderline" score
const CONFIDENCE_THRESHOLD_HIGH: f32 = 0.50; // Upper bound for "borderline" score

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
        let abs_mask = _mm256_castsi256_ps(_mm256_set1_epi32(0x7fffffff));
        let mask_high = _mm256_castsi256_ps(_mm256_set_epi32(0, 0, -1, -1, -1, -1, -1, -1));

        // 1. Scan L1 Root
        let mut best_l1 = [(f32::MAX, 0usize); N_PROBE_L1];
        let mut i = 0;
        while i + 3 < N_L1 {
            let (d0, d1, d2, d3) = self.dist_avx2_preloaded_x4(
                q_low, q_high, abs_mask, mask_high,
                &l1_centroids[i], &l1_centroids[i+1], &l1_centroids[i+2], &l1_centroids[i+3]
            );
            update_best_probes::<N_PROBE_L1>(&mut best_l1, d0, i);
            update_best_probes::<N_PROBE_L1>(&mut best_l1, d1, i + 1);
            update_best_probes::<N_PROBE_L1>(&mut best_l1, d2, i + 2);
            update_best_probes::<N_PROBE_L1>(&mut best_l1, d3, i + 3);
            i += 4;
        }

        // 2. Scan L2 Leaves for selected L1s
        let mut best_l2 = [(f32::MAX, 0usize); N_PROBE_L2_EXTENDED];
        for probe_idx in 0..N_PROBE_L1 {
            let l1_idx = best_l1[probe_idx].1;
            let l2_base = l1_idx * N_L2_PER_L1;
            let mut j = 0;
            while j + 7 < N_L2_PER_L1 {
                // SIMD Horizontal Pruning (Block of 8)
                // We calculate distances for 8 centroids and update best_l2.
                // The dist_avx2_preloaded_x4 is already very fast.
                let (d0, d1, d2, d3) = self.dist_avx2_preloaded_x4(
                    q_low, q_high, abs_mask, mask_high,
                    &l2_centroids[l2_base + j], &l2_centroids[l2_base + j + 1], 
                    &l2_centroids[l2_base + j + 2], &l2_centroids[l2_base + j + 3]
                );
                let (d4, d5, d6, d7) = self.dist_avx2_preloaded_x4(
                    q_low, q_high, abs_mask, mask_high,
                    &l2_centroids[l2_base + j + 4], &l2_centroids[l2_base + j + 5], 
                    &l2_centroids[l2_base + j + 6], &l2_centroids[l2_base + j + 7]
                );

                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, d0, l2_base + j);
                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, d1, l2_base + j + 1);
                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, d2, l2_base + j + 2);
                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, d3, l2_base + j + 3);
                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, d4, l2_base + j + 4);
                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, d5, l2_base + j + 5);
                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, d6, l2_base + j + 6);
                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, d7, l2_base + j + 7);
                
                j += 8;
            }
        }

        // 3. Scan Records in top 64 clusters (Initial Probe)
        let mut top_k = [(f32::MAX, 0u8, 0usize); K];
        let mut max_dist = f32::MAX;

        for probe_idx in 0..N_PROBE_L2 {
            max_dist = self.scan_cluster_dedup(best_l2[probe_idx].1, records, offsets, q_low, q_high, abs_mask, mask_high, &mut top_k, max_dist);
        }

        let (approved, score) = self.calculate_fraud_score_dedup(top_k);

        // 4. Adaptive Probing: If score is borderline, expand search
        if (score > CONFIDENCE_THRESHOLD_LOW && score < CONFIDENCE_THRESHOLD_HIGH) || top_k[0].0 > 1.5 {
            let mut extended_top_k = top_k;
            let mut extended_max_dist = max_dist;
            for probe_idx in N_PROBE_L2..N_PROBE_L2_EXTENDED {
                if best_l2[probe_idx].0 == f32::MAX { break; }
                extended_max_dist = self.scan_cluster_dedup(best_l2[probe_idx].1, records, offsets, q_low, q_high, abs_mask, mask_high, &mut extended_top_k, extended_max_dist);
            }
            return self.calculate_fraud_score_dedup(extended_top_k);
        }

        (approved, score)
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn scan_cluster_dedup(
        &self,
        l2_idx: usize,
        records: &[Record],
        offsets: &[u32],
        q_low: __m256,
        q_high: __m256,
        abs_mask: __m256,
        mask_high: __m256,
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
            let (d0, d1, d2, d3) = self.dist_avx2_preloaded_x4(
                q_low, q_high, abs_mask, mask_high,
                &cluster_records[k].vector, &cluster_records[k+1].vector, 
                &cluster_records[k+2].vector, &cluster_records[k+3].vector
            );

            // Record de-duplication using original index stored in padding vector[14]
            if d0 < max_dist { max_dist = update_top_k_dedup(top_k, d0, cluster_records[k].vector[15] as u8, cluster_records[k].vector[14] as u32 as usize); }
            if d1 < max_dist { max_dist = update_top_k_dedup(top_k, d1, cluster_records[k+1].vector[15] as u8, cluster_records[k+1].vector[14] as u32 as usize); }
            if d2 < max_dist { max_dist = update_top_k_dedup(top_k, d2, cluster_records[k+2].vector[15] as u8, cluster_records[k+2].vector[14] as u32 as usize); }
            if d3 < max_dist { max_dist = update_top_k_dedup(top_k, d3, cluster_records[k+3].vector[15] as u8, cluster_records[k+3].vector[14] as u32 as usize); }
            k += 4;
        }
        while k < cluster_records.len() {
            let dist = self.dist_avx2_preloaded(q_low, q_high, abs_mask, mask_high, &cluster_records[k].vector);
            if dist < max_dist {
                max_dist = update_top_k_dedup(top_k, dist, cluster_records[k].vector[15] as u8, cluster_records[k].vector[14] as u32 as usize);
            }
            k += 1;
        }
        max_dist
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
        let mut best_l1 = [(f32::MAX, 0usize); N_PROBE_L1];
        for i in 0..N_L1 {
            let dist = manhattan_distance_scalar(query, &self.data.l1_centroids[i]);
            update_best_probes::<N_PROBE_L1>(&mut best_l1, dist, i);
        }

        let mut best_l2 = [(f32::MAX, 0usize); N_PROBE_L2_EXTENDED];
        for probe_idx in 0..N_PROBE_L1 {
            let l1_idx = best_l1[probe_idx].1;
            let l2_base = l1_idx * N_L2_PER_L1;
            for j in 0..N_L2_PER_L1 {
                let dist = manhattan_distance_scalar(query, &self.data.l2_centroids[l2_base + j]);
                update_best_probes::<N_PROBE_L2_EXTENDED>(&mut best_l2, dist, l2_base + j);
            }
        }

        let mut top_k = [(f32::MAX, 0u8, 0usize); K];
        let mut max_dist = f32::MAX;
        for probe_idx in 0..N_PROBE_L2 {
            let l2_idx = best_l2[probe_idx].1;
            let start = self.data.offsets[l2_idx] as usize;
            let end = self.data.offsets[l2_idx + 1] as usize;
            for record in &records[start..end] {
                let dist = manhattan_distance_scalar(query, &record.vector);
                if dist < max_dist {
                    max_dist = update_top_k_dedup(&mut top_k, dist, record.vector[15] as u8, record.vector[14] as u32 as usize);
                }
            }
        }
        
        let (_, score) = self.calculate_fraud_score_dedup(top_k);
        if (score > CONFIDENCE_THRESHOLD_LOW && score < CONFIDENCE_THRESHOLD_HIGH) || top_k[0].0 > 1.5 {
            for probe_idx in N_PROBE_L2..N_PROBE_L2_EXTENDED {
                if best_l2[probe_idx].0 == f32::MAX { break; }
                let l2_idx = best_l2[probe_idx].1;
                let start = self.data.offsets[l2_idx] as usize;
                let end = self.data.offsets[l2_idx + 1] as usize;
                for record in &records[start..end] {
                    let dist = manhattan_distance_scalar(query, &record.vector);
                    if dist < max_dist {
                        max_dist = update_top_k_dedup(&mut top_k, dist, record.vector[15] as u8, record.vector[14] as u32 as usize);
                    }
                }
            }
        }

        self.calculate_fraud_score_dedup(top_k)
    }

    fn calculate_fraud_score_dedup(&self, top_k: [(f32, u8, usize); K]) -> (bool, f32) {
        let mut fraud_weight = 0.0f32;
        let mut total_weight = 0.0f32;
        for &(dist, label, _) in &top_k {
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
fn update_best_probes<const N: usize>(best: &mut [(f32, usize); N], dist: f32, idx: usize) {
    if dist < best[N - 1].0 {
        let mut pos = N - 1;
        while pos > 0 && dist < best[pos - 1].0 {
            best[pos] = best[pos - 1];
            pos -= 1;
        }
        best[pos] = (dist, idx);
    }
}

#[inline(always)]
fn update_top_k_dedup(top_k: &mut [(f32, u8, usize); K], dist: f32, label: u8, id: usize) -> f32 {
    for i in 0..K {
        if top_k[i].2 == id && id != 0 {
            if dist < top_k[i].0 {
                top_k[i].0 = dist;
                let mut j = i;
                while j > 0 && top_k[j].0 < top_k[j-1].0 {
                    top_k.swap(j, j-1);
                    j -= 1;
                }
            }
            return top_k[K-1].0;
        }
    }

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

#[inline(always)]
fn manhattan_distance_scalar(v1: &[f32; 16], v2: &[f32; 16]) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 { sum += (v1[i] - v2[i]).abs(); }
    sum
}
