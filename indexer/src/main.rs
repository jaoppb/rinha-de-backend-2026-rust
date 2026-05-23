use flate2::read::GzDecoder;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use rand::prelude::*;

#[derive(Deserialize)]
struct JsonRecord {
    vector: [f32; 14],
    label: String,
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct Record {
    pub vector: [f32; 16],  // 64 bytes, label stored in vector[15]
}

const N_L1: usize = 256;
const N_L2_PER_L1: usize = 256;
const N_TOTAL_L2: usize = N_L1 * N_L2_PER_L1;
const N_ITERATIONS: usize = 10;
const SOFT_ASSIGNMENT_COUNT: usize = 4; // Map each record to its top 4 nearest L2 clusters

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: indexer <input_file> <output_dir>");
        std::process::exit(1);
    }
    let input_path = &args[1];
    let output_dir = &args[2];

    println!("Opening input file: {}", input_path);
    let mut file_read = File::open(input_path)?;
    
    // Detect GZIP
    let mut magic = [0u8; 2];
    let is_gzip = if file_read.read_exact(&mut magic).is_ok() {
        magic == [0x1f, 0x8b]
    } else {
        false
    };
    
    let mut records: Vec<Record> = if is_gzip {
        println!("Detected GZIP format");
        let file = File::open(input_path)?;
        let decoder = GzDecoder::new(file);
        let reader = BufReader::new(decoder);
        let json_records: Vec<JsonRecord> = serde_json::from_reader(reader)?;
        json_records.into_iter().map(convert_record).collect()
    } else {
        println!("Detected plain JSON format");
        let file = File::open(input_path)?;
        let reader = BufReader::new(file);
        let json_records: Vec<JsonRecord> = serde_json::from_reader(reader)?;
        json_records.into_iter().map(convert_record).collect()
    };

    // Set original record index in padding for de-duplication
    for (i, r) in records.iter_mut().enumerate() {
        r.vector[14] = i as f32;
    }

    println!("Successfully loaded {} records", records.len());
    std::fs::create_dir_all(output_dir)?;

    // 1. L1 Clustering
    println!("Clustering L1 ({} super-centroids, {} iterations)...", N_L1, N_ITERATIONS);
    let mut l1_centroids = init_kmeans_plus_plus(&records, N_L1);

    let mut l1_assignments = vec![0usize; records.len()];
    train_k_means(&records, &mut l1_centroids, &mut l1_assignments, N_ITERATIONS);

    // 2. L2 Clustering
    println!("Clustering L2 ({} sub-centroids per L1, total {})...", N_L2_PER_L1, N_TOTAL_L2);
    let mut all_l2_centroids = vec![[0.0f32; 16]; N_TOTAL_L2];
    let mut record_l2_multi_assignments = vec![Vec::with_capacity(2); records.len()];

    for l1_idx in 0..N_L1 {
        let l1_record_indices: Vec<usize> = l1_assignments.iter()
            .enumerate()
            .filter(|&(_, &idx)| idx == l1_idx)
            .map(|(i, _)| i)
            .collect();

        if l1_record_indices.is_empty() {
            continue;
        }

        let l1_records: Vec<Record> = l1_record_indices.iter()
            .map(|&i| records[i])
            .collect();

        let mut l2_centroids = init_kmeans_plus_plus(&l1_records, N_L2_PER_L1);
        let mut l2_assignments = vec![0usize; l1_records.len()];
        train_k_means(&l1_records, &mut l2_centroids, &mut l2_assignments, N_ITERATIONS);

        // Copy back to global L2 centroids
        for i in 0..N_L2_PER_L1 {
            all_l2_centroids[l1_idx * N_L2_PER_L1 + i] = l2_centroids[i];
        }

        // Soft Assignment: Map each record in this L1 to top clusters
        for (local_idx, record) in l1_records.iter().enumerate() {
            let mut dists: Vec<(f32, usize)> = l2_centroids.iter()
                .enumerate()
                .map(|(c_idx, c)| (manhattan_distance(&record.vector, c), c_idx))
                .collect();
            
            dists.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

            let global_record_idx = l1_record_indices[local_idx];
            for i in 0..SOFT_ASSIGNMENT_COUNT.min(dists.len()) {
                record_l2_multi_assignments[global_record_idx].push(l1_idx * N_L2_PER_L1 + dists[i].1);
            }
        }

        if l1_idx % 32 == 0 {
            println!("Processed L1 cluster {}/{}", l1_idx, N_L1);
        }
    }

    // 3. Write artifacts
    println!("Writing artifacts...");
    
    // Flatten record multi-assignments into (cluster_idx, record_idx) pairs
    let mut flat_assignments = Vec::with_capacity(records.len() * 2);
    for (r_idx, clusters) in record_l2_multi_assignments.iter().enumerate() {
        for &c_idx in clusters {
            flat_assignments.push((c_idx, r_idx));
        }
    }
    
    // Sort by cluster index for contiguous disk access in API
    flat_assignments.sort_by_key(|&(c, _)| c);
    
    let mut offsets = vec![0u32; N_TOTAL_L2 + 1];
    let mut current_offset = 0;
    let mut final_records = Vec::with_capacity(flat_assignments.len());
    
    let mut current_c = 0;
    offsets[0] = 0;
    for (c, i) in flat_assignments {
        while current_c < c {
            current_c += 1;
            offsets[current_c] = current_offset;
        }
        final_records.push(records[i]);
        current_offset += 1;
    }
    while current_c < N_TOTAL_L2 {
        current_c += 1;
        offsets[current_c] = current_offset;
    }

    let dataset_path = Path::new(output_dir).join("dataset.bin");
    let mut f = File::create(&dataset_path)?;
    for record in &final_records {
        let bytes = unsafe { std::slice::from_raw_parts((record as *const Record) as *const u8, 64) };
        f.write_all(bytes)?;
    }

    let l1_path = Path::new(output_dir).join("l1_centroids.bin");
    let mut f = File::create(&l1_path)?;
    for c in &l1_centroids {
        let bytes = unsafe { std::slice::from_raw_parts(c.as_ptr() as *const u8, 64) };
        f.write_all(bytes)?;
    }

    let l2_path = Path::new(output_dir).join("l2_centroids.bin");
    let mut f = File::create(&l2_path)?;
    for c in &all_l2_centroids {
        let bytes = unsafe { std::slice::from_raw_parts(c.as_ptr() as *const u8, 64) };
        f.write_all(bytes)?;
    }

    let offsets_path = Path::new(output_dir).join("offsets.bin");
    let mut f = File::create(&offsets_path)?;
    for &o in &offsets {
        f.write_all(&o.to_ne_bytes())?;
    }

    println!("All artifacts created in {}. Final dataset size: {} records (Soft Assignment)", 
        output_dir, final_records.len());
    Ok(())
}

fn init_kmeans_plus_plus(records: &[Record], n_clusters: usize) -> Vec<[f32; 16]> {
    if records.is_empty() { return vec![[0.0; 16]; n_clusters]; }
    let mut rng = StdRng::seed_from_u64(42);
    let mut centroids = Vec::with_capacity(n_clusters);

    // 1. Pick first centroid at random
    let first_idx = rng.gen_range(0..records.len());
    centroids.push(records[first_idx].vector);

    // 2. Pick remaining centroids
    let mut min_dists = vec![f32::MAX; records.len()];
    for _ in 1..n_clusters {
        let last_centroid = centroids.last().unwrap();
        let mut total_dist = 0.0;
        
        for (i, record) in records.iter().enumerate() {
            let d = manhattan_distance(&record.vector, last_centroid);
            if d < min_dists[i] {
                min_dists[i] = d;
            }
            total_dist += min_dists[i] * min_dists[i]; // Probability proportional to distance squared
        }

        let mut threshold = rng.r#gen::<f32>() * total_dist;
        let mut chosen_idx = records.len() - 1;
        for (i, &d) in min_dists.iter().enumerate() {
            threshold -= d * d;
            if threshold <= 0.0 {
                chosen_idx = i;
                break;
            }
        }
        centroids.push(records[chosen_idx].vector);
    }

    centroids
}

fn train_k_means(records: &[Record], centroids: &mut [[f32; 16]], assignments: &mut [usize], iterations: usize) {
    let n_centroids = centroids.len();
    
    for _iter in 0..iterations {
        for (i, record) in records.iter().enumerate() {
            let mut min_dist = f32::MAX;
            let mut best_c = 0;
            for (c_idx, centroid) in centroids.iter().enumerate() {
                let dist = manhattan_distance(&record.vector, centroid);
                if dist < min_dist {
                    min_dist = dist;
                    best_c = c_idx;
                }
            }
            assignments[i] = best_c;
        }

        let mut new_centroids = vec![[0.0f32; 16]; n_centroids];
        let mut counts = vec![0usize; n_centroids];
        for (i, &assignment) in assignments.iter().enumerate() {
            for j in 0..14 {
                new_centroids[assignment][j] += records[i].vector[j];
            }
            counts[assignment] += 1;
        }
        for i in 0..n_centroids {
            if counts[i] > 0 {
                for j in 0..14 {
                    new_centroids[i][j] /= counts[i] as f32;
                }
                centroids[i] = new_centroids[i];
            }
        }
    }
}

fn convert_record(jr: JsonRecord) -> Record {
    let mut v = [0.0f32; 16];
    for i in 0..14 { v[i] = jr.vector[i]; }
    v[14] = 0.0; // Will be set to original index in main loop
    v[15] = if jr.label == "fraud" { 1.0 } else { 0.0 };
    Record { vector: v }
}

fn manhattan_distance(v1: &[f32; 16], v2: &[f32; 16]) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 { sum += (v1[i] - v2[i]).abs(); }
    sum
}
