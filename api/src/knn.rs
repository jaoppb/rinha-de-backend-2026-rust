use crate::mmap::{IvfData, Record};
use std::arch::x86_64::*;

const K: usize = 7;
const N_CENTROIDS: usize = 2048;
const N_PROBE: usize = 32; // Increased for better accuracy with 2048 centroids
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

        // 1. Load query once into registers
        let q_low = _mm256_loadu_ps(query.as_ptr());
        let q_high = _mm256_loadu_ps(query.as_ptr().add(8));
        let abs_mask = _mm256_castsi256_ps(_mm256_set1_epi32(0x7fffffff));

        // 2. Find nearest clusters (probes)
        let mut best_probes = [(f32::MAX, 0usize); N_PROBE];
        
        for i in 0..N_CENTROIDS {
            let dist = self.dist_avx2_preloaded(q_low, q_high, abs_mask, &centroids[i]);
            
            if dist < best_probes[N_PROBE - 1].0 {
                let mut pos = N_PROBE - 1;
                while pos > 0 && dist < best_probes[pos - 1].0 {
                    best_probes[pos] = best_probes[pos - 1];
                    pos -= 1;
                }
                best_probes[pos] = (dist, i);
            }
        }

        // 3. Search nearest records in probes
        let mut top_k = [(f32::MAX, 0u8); K];
        let mut max_dist = f32::MAX;

        for i in 0..N_PROBE {
            let (_, c_idx) = best_probes[i];
            let start = offsets[c_idx] as usize;
            let end = offsets[c_idx + 1] as usize;
            
            let cluster_records = &records[start..end];

            let mut k = 0;
            // Unroll loop to process 2 records at a time for better ILP
            while k + 1 < cluster_records.len() {
                let r1 = &cluster_records[k];
                let r2 = &cluster_records[k + 1];
                
                // Prefetch ahead
                if k + 8 < cluster_records.len() {
                    _mm_prefetch(cluster_records[k + 8].vector.as_ptr() as *const i8, _MM_HINT_T0);
                }

                let dist1 = self.dist_avx2_preloaded(q_low, q_high, abs_mask, &r1.vector);
                let dist2 = self.dist_avx2_preloaded(q_low, q_high, abs_mask, &r2.vector);

                if dist1 < max_dist {
                    max_dist = update_top_k(&mut top_k, dist1, r1.vector[15] as u8);
                }
                if dist2 < max_dist {
                    max_dist = update_top_k(&mut top_k, dist2, r2.vector[15] as u8);
                }
                k += 2;
            }
            // Handle remaining
            if k < cluster_records.len() {
                let r = &cluster_records[k];
                let dist = self.dist_avx2_preloaded(q_low, q_high, abs_mask, &r.vector);
                if dist < max_dist {
                    max_dist = update_top_k(&mut top_k, dist, r.vector[15] as u8);
                }
            }
        }

        self.calculate_fraud_score(top_k)
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn dist_avx2_preloaded(&self, q_low: __m256, q_high: __m256, abs_mask: __m256, v2: &[f32; 16]) -> f32 {
        let b_low = _mm256_loadu_ps(v2.as_ptr());
        let b_high = _mm256_loadu_ps(v2.as_ptr().add(8));
        
        let abs_low = _mm256_and_ps(_mm256_sub_ps(q_low, b_low), abs_mask);
        let abs_high = _mm256_and_ps(_mm256_sub_ps(q_high, b_high), abs_mask);
        
        // Horizontal sum of first 14 dimensions. 
        // dimension 14 and 15 should be 0 in query. 
        // In Record, dimension 15 is label, dimension 14 is 0.
        // So we sum 16 dimensions and it will be (sum of 14) + |0-0| + |0-label| = (sum of 14) + label.
        // To be exact, we should mask out dimension 14 and 15.
        
        let sum_vec = _mm256_add_ps(abs_low, abs_high);
        
        // Mask out last 2 elements of high part (index 14 and 15)
        // This is important because vector[15] is the label (0 or 1).
        let mask_high = _mm256_castsi256_ps(_mm256_set_epi32(0, 0, -1, -1, -1, -1, -1, -1));
        let masked_high = _mm256_and_ps(abs_high, mask_high);
        let sum_vec = _mm256_add_ps(abs_low, masked_high);

        // Horizontal sum
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
        let centroids = self.data.centroids;
        let offsets = self.data.offsets;

        let mut best_probes = [(f32::MAX, 0usize); N_PROBE];
        for i in 0..N_CENTROIDS {
            let dist = manhattan_distance_scalar(query, &centroids[i], f32::MAX);
            if dist < best_probes[N_PROBE - 1].0 {
                let mut pos = N_PROBE - 1;
                while pos > 0 && dist < best_probes[pos - 1].0 {
                    best_probes[pos] = best_probes[pos - 1];
                    pos -= 1;
                }
                best_probes[pos] = (dist, i);
            }
        }

        let mut top_k = [(f32::MAX, 0u8); K];
        let mut max_dist = f32::MAX;

        for i in 0..N_PROBE {
            let (_, c_idx) = best_probes[i];
            let start = offsets[c_idx] as usize;
            let end = offsets[c_idx + 1] as usize;

            for record in &records[start..end] {
                let dist = manhattan_distance_scalar(query, &record.vector, max_dist);

                if dist < max_dist {
                    max_dist = update_top_k(&mut top_k, dist, record.vector[15] as u8);
                }
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

        let fraud_score = if total_weight > 0.0 {
            fraud_weight / total_weight
        } else {
            0.0
        };

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

#[inline(always)]
fn manhattan_distance_scalar(v1: &[f32; 16], v2: &[f32; 16], limit: f32) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 { // Only first 14 dimensions are features
        sum += (v1[i] - v2[i]).abs();
        if sum >= limit {
            return sum;
        }
    }
    sum
}
