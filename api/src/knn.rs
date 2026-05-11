use crate::mmap::{Record, IvfData};

const K: usize = 5;
const N_CENTROIDS: usize = 256;
const N_PROBE: usize = 8; // Number of clusters to search

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
            cluster_dists[i] = (i, euclidean_distance(query, centroid));
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
                let dist = euclidean_distance(query, &record.vector);

                // Update top K
                if dist < top_k[K - 1].0 {
                    top_k[K - 1] = (dist, record.label);
                    top_k.sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                }
            }
        }

        // 3. Calculate score
        let mut fraud_count = 0;
        for i in 0..K {
            if top_k[i].1 == 1 {
                fraud_count += 1;
            }
        }

        let fraud_score = fraud_count as f32 / K as f32;
        (fraud_score < 0.6, fraud_score)
    }
}

#[inline(always)]
fn euclidean_distance(v1: &[f32; 14], v2: &[f32; 14]) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 {
        let diff = v1[i] - v2[i];
        sum += diff * diff;
    }
    sum
}
