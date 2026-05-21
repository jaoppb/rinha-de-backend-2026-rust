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
    let mut sum = 0.0;
    // Unrolling or encouraging auto-vec by using chunks or fixed sizes
    for i in 0..14 {
        sum += (v1[i] - v2[i]).abs();
        if sum >= limit {
            return sum;
        }
    }
    sum
}
