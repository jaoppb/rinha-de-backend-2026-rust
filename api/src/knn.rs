use crate::mmap::Record;

const K: usize = 5;
const N_CENTROIDS: usize = 256;
const N_PROBE: usize = 8; // Number of clusters to search

pub struct IvfIndex {
    pub centroids: [[f32; 14]; N_CENTROIDS],
    pub indices: Box<[u32]>,
    pub offsets: [u32; N_CENTROIDS + 1],
}

impl IvfIndex {
    pub fn build(records: &[Record]) -> Self {
        // 1. Initialize centroids (simple: pick first N_CENTROIDS or a stride)
        let mut centroids = [[0.0f32; 14]; N_CENTROIDS];
        let stride = records.len() / N_CENTROIDS;
        for i in 0..N_CENTROIDS {
            centroids[i] = records[i * stride].vector;
        }

        // 2. Assign each record to its nearest centroid
        let mut assignments = vec![0u16; records.len()];
        let mut counts = [0u32; N_CENTROIDS];

        for (i, record) in records.iter().enumerate() {
            let mut min_dist = f32::MAX;
            let mut best_c = 0;
            for (c_idx, centroid) in centroids.iter().enumerate() {
                let dist = euclidean_distance(&record.vector, centroid);
                if dist < min_dist {
                    min_dist = dist;
                    best_c = c_idx;
                }
            }
            assignments[i] = best_c as u16;
            counts[best_c] += 1;
        }

        // 3. Create CSR structure
        let mut offsets = [0u32; N_CENTROIDS + 1];
        for i in 0..N_CENTROIDS {
            offsets[i + 1] = offsets[i] + counts[i];
        }

        let mut indices = vec![0u32; records.len()].into_boxed_slice();
        let mut current_pos = offsets; // Copy to track insertion points
        for (i, &c_idx) in assignments.iter().enumerate() {
            let pos = current_pos[c_idx as usize];
            indices[pos as usize] = i as u32;
            current_pos[c_idx as usize] += 1;
        }

        Self {
            centroids,
            indices,
            offsets,
        }
    }

    pub fn search(&self, query: &[f32; 14], records: &[Record]) -> (bool, f32) {
        // 1. Find nearest clusters
        let mut cluster_dists = [(0usize, 0.0f32); N_CENTROIDS];
        for (i, centroid) in self.centroids.iter().enumerate() {
            cluster_dists[i] = (i, euclidean_distance(query, centroid));
        }
        cluster_dists.sort_unstable_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // 2. Search nearest probes
        let mut top_k = [(f32::MAX, 0u8); K]; // (distance, label)

        for i in 0..N_PROBE {
            let (c_idx, _) = cluster_dists[i];
            let start = self.offsets[c_idx] as usize;
            let end = self.offsets[c_idx + 1] as usize;

            for &idx in &self.indices[start..end] {
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
    sum // We can skip sqrt for comparison/ranking
}
