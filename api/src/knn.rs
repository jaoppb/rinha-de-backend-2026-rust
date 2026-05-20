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

        // 1. Find nearest clusters
        let mut cluster_dists = [(0usize, 0.0f32); N_CENTROIDS];
        for (i, centroid) in centroids.iter().enumerate() {
            cluster_dists[i] = (i, manhattan_distance(query, centroid));
        }
        cluster_dists.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // 2. Search nearest probes
        let mut top_k = [(f32::MAX, 0u8); K]; // (distance, label)

        for i in 0..N_PROBE {
            let (c_idx, _) = cluster_dists[i];
            let start = offsets[c_idx] as usize;
            let end = offsets[c_idx + 1] as usize;

            for &idx in &indices[start..end] {
                let record = &records[idx as usize];
                let dist = manhattan_distance(query, &record.vector);

                // Update top K
                if dist < top_k[K - 1].0 {
                    top_k[K - 1] = (dist, record.label);
                    top_k.sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                }
            }
        }

        // 3. Calculate score with distance-weighted voting.
        let mut fraud_weight = 0.0f32;
        let mut total_weight = 0.0f32;
        for &(dist, label) in &top_k {
            let weight = 1.0 / (1.0 + dist.max(0.0) + SCORE_EPS);
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
fn manhattan_distance(v1: &[f32; 14], v2: &[f32; 14]) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 {
        sum += (v1[i] - v2[i]).abs();
    }
    sum
}
