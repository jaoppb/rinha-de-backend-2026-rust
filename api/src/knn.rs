use crate::mmap::{IvfData, Record};

const K: usize = 7;
const N_CENTROIDS: usize = 256;
const N_PROBE: usize = 10; // Number of clusters to search
const SCORE_EPS: f32 = 1e-6;
const APPROVAL_THRESHOLD: f32 = 0.44;

pub struct IvfIndex {
    data: IvfData,
}

impl IvfIndex {
    pub fn new(data: IvfData) -> Self {
        Self { data }
    }

    pub fn search(&self, query: &[f32; 14], records: &[Record]) -> (bool, f32) {
        let centroids = self.data.centroids;
        let indices = self.data.indices;
        let offsets = self.data.offsets;

        // 1. Find nearest clusters - Optimization: use select_nth_unstable instead of full sort
        let mut cluster_dists = [(0usize, 0.0f32); N_CENTROIDS];
        for (i, centroid) in centroids.iter().enumerate() {
            cluster_dists[i] = (i, manhattan_distance(query, centroid, f32::MAX));
        }
        // We only need the top N_PROBE, so select_nth_unstable is O(N) instead of O(N log N)
        let (probes, _, _) = cluster_dists.select_nth_unstable_by(N_PROBE, |a, b| a.1.partial_cmp(&b.1).unwrap());
        probes.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // 2. Search nearest probes
        let mut top_k = [(f32::MAX, 0u8); K]; // (distance, label)
        let mut max_dist = f32::MAX;

        for i in 0..N_PROBE {
            let (c_idx, _) = probes[i];
            let start = offsets[c_idx] as usize;
            let end = offsets[c_idx + 1] as usize;

            for &idx in &indices[start..end] {
                let record = &records[idx as usize];
                // Optimization: Early exit if partial distance exceeds max_dist
                let dist = manhattan_distance(query, &record.vector, max_dist);

                if dist < max_dist {
                    // Update top K manually instead of full sort every time
                    // Since K is very small (7), a simple insertion is faster than sorting or a full heap
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
fn manhattan_distance(v1: &[f32; 14], v2: &[f32; 14], limit: f32) -> f32 {
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
unsafe fn manhattan_distance_avx2(v1: &[f32; 14], v2: &[f32; 14], limit: f32) -> f32 {
    use std::arch::x86_64::*;

    // Load first 8 floats
    let a8 = _mm256_loadu_ps(v1.as_ptr());
    let b8 = _mm256_loadu_ps(v2.as_ptr());
    
    // abs(a - b)
    let diff8 = _mm256_sub_ps(a8, b8);
    // Use bitwise AND with 0x7FFFFFFF to get absolute value
    let abs_mask = _mm256_castsi256_ps(_mm256_set1_epi32(0x7fffffff));
    let abs8 = _mm256_and_ps(diff8, abs_mask);
    
    // Horizontal sum of the 8 floats
    // This is slightly faster than extracting individually
    let mut sum8_buf = [0.0f32; 8];
    _mm256_storeu_ps(sum8_buf.as_mut_ptr(), abs8);
    let mut sum = sum8_buf[0] + sum8_buf[1] + sum8_buf[2] + sum8_buf[3] +
                  sum8_buf[4] + sum8_buf[5] + sum8_buf[6] + sum8_buf[7];

    if sum >= limit {
        return sum;
    }

    // Remaining 6 floats (14 - 8 = 6)
    for i in 8..14 {
        sum += (v1[i] - v2[i]).abs();
        if sum >= limit {
            return sum;
        }
    }

    sum
}

#[inline(always)]
fn manhattan_distance_scalar(v1: &[f32; 14], v2: &[f32; 14], limit: f32) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 {
        sum += (v1[i] - v2[i]).abs();
        if sum >= limit {
            return sum;
        }
    }
    sum
}
