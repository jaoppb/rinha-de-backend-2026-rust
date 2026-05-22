use crate::mmap::{IvfData, Record};

const K: usize = 7;
const N_CENTROIDS: usize = 256;
const N_PROBE: usize = 8; // Number of clusters to search
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
        let centroids = self.data.centroids;
        let indices = self.data.indices;
        let offsets = self.data.offsets;

        // 1. Find nearest clusters
        let mut cluster_dists = [(0usize, 0.0f32); N_CENTROIDS];
        for (i, centroid) in centroids.iter().enumerate() {
            cluster_dists[i] = (i, manhattan_distance(query, centroid, f32::MAX));
        }
        let (probes, _, _) = cluster_dists.select_nth_unstable_by(N_PROBE, |a, b| a.1.partial_cmp(&b.1).unwrap());
        probes.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // 2. Search nearest probes
        let mut top_k = [(f32::MAX, 0u8); K]; // (distance, label)
        let mut max_dist = f32::MAX;

        for i in 0..N_PROBE {
            let (c_idx, _) = probes[i];
            let start = offsets[c_idx] as usize;
            let end = offsets[c_idx + 1] as usize;

            let cluster_indices = &indices[start..end];
            for k in 0..cluster_indices.len() {
                let idx = cluster_indices[k];
                
                // Prefetch next record's vector (Record is 128 bytes, vector is the first 64 bytes)
                #[cfg(target_arch = "x86_64")]
                if k + 1 < cluster_indices.len() {
                    let next_idx = cluster_indices[k + 1] as usize;
                    unsafe {
                        use std::arch::x86_64::_mm_prefetch;
                        _mm_prefetch(records[next_idx].vector.as_ptr() as *const i8, std::arch::x86_64::_MM_HINT_T0);
                    }
                }

                let record = &records[idx as usize];
                let dist = manhattan_distance(query, &record.vector, max_dist);

                if dist < max_dist {
                    let mut pos = K - 1;
                    while pos > 0 && dist < top_k[pos - 1].0 {
                        top_k[pos] = top_k[pos - 1];
                        pos -= 1;
                    }
                    top_k[pos] = (dist, record.label);
                    max_dist = top_k[K - 1].0;
                }
            }
        }

        // 3. Calculate score with distance-weighted voting.
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
fn manhattan_distance(v1: &[f32; 16], v2: &[f32; 16], limit: f32) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { manhattan_distance_avx2(v1, v2, limit) };
        }
    }
    
    manhattan_distance_scalar(v1, v2, limit)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn manhattan_distance_avx2(v1: &[f32; 16], v2: &[f32; 16], _limit: f32) -> f32 {
    use std::arch::x86_64::*;

    // Load all 16 floats (Record vector is now 64-byte aligned and 64 bytes total)
    let a_low = _mm256_loadu_ps(v1.as_ptr());
    let a_high = _mm256_loadu_ps(v1.as_ptr().add(8));
    let b_low = _mm256_loadu_ps(v2.as_ptr());
    let b_high = _mm256_loadu_ps(v2.as_ptr().add(8));
    
    // abs(a - b)
    let abs_mask = _mm256_castsi256_ps(_mm256_set1_epi32(0x7fffffff));
    let abs_low = _mm256_and_ps(_mm256_sub_ps(a_low, b_low), abs_mask);
    let abs_high = _mm256_and_ps(_mm256_sub_ps(a_high, b_high), abs_mask);
    
    // Combine
    let sum_vec = _mm256_add_ps(abs_low, abs_high);
    
    // Fast horizontal sum of 8 floats
    let x128_low = _mm256_castps256_ps128(sum_vec);
    let x128_high = _mm256_extractf128_ps(sum_vec, 1);
    let x_sum = _mm_add_ps(x128_low, x128_high);
    
    // Horizontal sum of 4 floats in x_sum
    let x_shuf = _mm_movehdup_ps(x_sum);
    let x_sum2 = _mm_add_ps(x_sum, x_shuf);
    let x_shuf2 = _mm_movehl_ps(x_sum2, x_sum2);
    let x_final = _mm_add_ss(x_sum2, x_shuf2);
    
    _mm_cvtss_f32(x_final)
}

#[inline(always)]
fn manhattan_distance_scalar(v1: &[f32; 16], v2: &[f32; 16], limit: f32) -> f32 {
    let mut sum = 0.0;
    for i in 0..16 {
        sum += (v1[i] - v2[i]).abs();
        if sum >= limit {
            return sum;
        }
    }
    sum
}
