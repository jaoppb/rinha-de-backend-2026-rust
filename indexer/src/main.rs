use flate2::read::GzDecoder;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use rand::prelude::*;
use rayon::prelude::*;

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
const N_ITERATIONS: usize = 20;

static FEATURE_WEIGHTS: [f32; 16] = [
    1.0038165, 0.665417, 0.8668326, 0.5379362, 0.5, 0.3, 0.3701757, 1.0, 1.2, 1.2648705, 0.81239825, 1.051987, 0.8247206, 2.0315619, 0.0, 0.0,
];

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

    // Set original record index in padding for de-duplication (bit 0: label, bits 1-31: id)
    for (i, r) in records.iter_mut().enumerate() {
        let label_bit = if r.vector[15] > 0.5 { 1u32 } else { 0u32 };
        let id_bits = (i as u32) << 1;
        let packed = id_bits | label_bit;
        r.vector[15] = f32::from_bits(packed);
    }

    println!("Successfully loaded {} records", records.len());
    std::fs::create_dir_all(output_dir)?;

    if args.len() > 3 && args[3] == "tune" {
        run_tuning_loop(&records);
        return Ok(());
    }

    // 1. L1 Clustering
    println!("Clustering L1 ({} super-centroids, {} iterations)...", N_L1, N_ITERATIONS);
    let mut l1_centroids = init_kmeans_plus_plus(&records, N_L1);

    let mut l1_assignments = vec![0usize; records.len()];
    train_k_means(&records, &mut l1_centroids, &mut l1_assignments, N_ITERATIONS);

    // Group records by L1 for parallel L2 processing
    let mut l1_groups: Vec<Vec<(usize, Record)>> = vec![Vec::new(); N_L1];
    for (i, &l1_idx) in l1_assignments.iter().enumerate() {
        l1_groups[l1_idx].push((i, records[i]));
    }

    // 2. L2 Clustering (Parallel)
    println!("Clustering L2 ({} sub-centroids per L1, total {})...", N_L2_PER_L1, N_TOTAL_L2);
    
    let l2_results: Vec<_> = l1_groups.into_par_iter().enumerate().map(|(l1_idx, group)| {
        if group.is_empty() {
            return (l1_idx, vec![[0.0f32; 16]; N_L2_PER_L1], vec![]);
        }

        let l1_records: Vec<Record> = group.iter().map(|&(_, r)| r).collect();
        let mut l2_centroids = init_kmeans_plus_plus(&l1_records, N_L2_PER_L1);
        let mut l2_assignments = vec![0usize; l1_records.len()];
        train_k_means(&l1_records, &mut l2_centroids, &mut l2_assignments, N_ITERATIONS);

        // Map back to absolute indices and cluster IDs
        let global_assignments: Vec<(usize, usize)> = l2_assignments.into_iter().enumerate().map(|(local_idx, best_l2)| {
            let global_record_idx = group[local_idx].0;
            let absolute_l2_idx = l1_idx * N_L2_PER_L1 + best_l2;
            (global_record_idx, absolute_l2_idx)
        }).collect();

        (l1_idx, l2_centroids, global_assignments)
    }).collect();

    // Reconstruct global L2 state
    let mut all_l2_centroids = vec![[0.0f32; 16]; N_TOTAL_L2];
    let mut record_l2_assignments = vec![0usize; records.len()];
    for (l1_idx, l2_centroids, assignments) in l2_results {
        for i in 0..N_L2_PER_L1 {
            all_l2_centroids[l1_idx * N_L2_PER_L1 + i] = l2_centroids[i];
        }
        for (global_idx, l2_idx) in assignments {
            record_l2_assignments[global_idx] = l2_idx;
        }
    }

    // 3. Write artifacts
    println!("Writing artifacts...");
    
    // Sort by cluster index for contiguous disk access
    let mut assignment_pairs: Vec<(usize, usize)> = record_l2_assignments.iter()
        .enumerate()
        .map(|(r_idx, &c_idx)| (c_idx, r_idx))
        .collect();
    
    assignment_pairs.sort_by_key(|&(c, _)| c);
    
    let mut offsets = vec![0u32; N_TOTAL_L2 + 1];
    let mut current_offset = 0;
    let mut final_records = Vec::with_capacity(records.len());
    
    let mut current_c = 0;
    offsets[0] = 0;
    for (c, i) in assignment_pairs {
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

    println!("All artifacts created in {}. Final dataset size: {} records", 
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
        
        min_dists.par_iter_mut().enumerate().for_each(|(i, d)| {
            let record = &records[i];
            let dist = manhattan_distance_weighted(&record.vector, last_centroid);
            if dist < *d {
                *d = dist;
            }
        });

        let total_dist: f32 = min_dists.iter().map(|d| d * d).sum();
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
        assignments.par_iter_mut().enumerate().for_each(|(i, best_c)| {
            let record = &records[i];
            let mut min_dist = f32::MAX;
            let mut best = 0;
            for (c_idx, centroid) in centroids.iter().enumerate() {
                let dist = manhattan_distance_weighted(&record.vector, centroid);
                if dist < min_dist {
                    min_dist = dist;
                    best = c_idx;
                }
            }
            *best_c = best;
        });

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

fn run_tuning_loop(records: &[Record]) {
    println!("Starting Weight Tuning (Brute Force Mode)...");
    let val_count = 5000;
    let mut rng = StdRng::seed_from_u64(42);
    
    // Reservoir Sample for Validation Set
    let mut val_indices: Vec<usize> = (0..records.len()).collect();
    val_indices.shuffle(&mut rng);
    let val_set: Vec<Record> = val_indices[0..val_count].iter().map(|&i| records[i]).collect();
    let ref_set: Vec<Record> = records.iter().enumerate()
        .filter(|(i, _)| !val_indices[0..val_count].contains(i))
        .map(|(_, &r)| r).collect();

    println!("Validation Set: {} records | Reference Set: {} records", val_set.len(), ref_set.len());

    let mut current_weights = FEATURE_WEIGHTS;
    let mut best_mcc = -1.0;
    let mut best_weights = current_weights;

    for iteration in 0..1000 {
        let candidate_weights = if iteration == 0 {
            current_weights
        } else {
            let mut w = best_weights;
            // Randomly mutate 1-3 weights
            for _ in 0..rng.gen_range(1..=3) {
                let idx = rng.gen_range(0..14);
                let mutation = rng.gen_range(0.8..1.2);
                w[idx] *= mutation;
            }
            w
        };

        // Evaluation (Parallel Brute Force)
        let results: Vec<(f32, u8)> = val_set.par_iter().map(|val_record| {
            let mut top_k = [(f32::MAX, 0u8); 7]; // (dist, label)
            let query = &val_record.vector;

            for ref_record in &ref_set {
                let mut dist = 0.0;
                for i in 0..14 {
                    dist += (query[i] - ref_record.vector[i]).abs() * candidate_weights[i];
                }
                
                if dist < top_k[6].0 {
                    let mut pos = 6;
                    while pos > 0 && dist < top_k[pos - 1].0 {
                        top_k[pos] = top_k[pos - 1];
                        pos -= 1;
                    }
                    let packed = ref_record.vector[15].to_bits();
                    let label = (packed & 1) as u8;
                    top_k[pos] = (dist, label);
                }
            }

            // Calculate Fraud Score (Weighted)
            let mut fraud_weight = 0.0;
            let mut total_weight = 0.0;
            for &(d, l) in &top_k {
                let weight = (-d * 0.5).exp();
                total_weight += weight;
                fraud_weight += weight * l as f32;
            }
            let score = if total_weight > 0.0 { fraud_weight / total_weight } else { 0.0 };
            let packed_val = val_record.vector[15].to_bits();
            let true_label = (packed_val & 1) as u8;
            (score, true_label)
        }).collect();

        // Calculate MCC (Matthews Correlation Coefficient)
        let mut tp = 0.0; let mut tn = 0.0; let mut fp = 0.0; let mut fn_ = 0.0;
        let threshold = 0.44; // Matches APPROVAL_THRESHOLD
        for (score, true_label) in results {
            let predicted = if score >= threshold { 1 } else { 0 };
            match (predicted, true_label) {
                (1, 1) => tp += 1.0,
                (0, 0) => tn += 1.0,
                (1, 0) => fp += 1.0,
                (0, 1) => fn_ += 1.0,
                _ => unreachable!(),
            }
        }

        let mcc = if (tp + fp) * (tp + fn_) * (tn + fp) * (tn + fn_) == 0.0 {
            0.0
        } else {
            (tp * tn - fp * fn_) / ((tp + fp) * (tp + fn_) * (tn + fp) * (tn + fn_) as f32).sqrt()
        };

        if mcc > best_mcc {
            best_mcc = mcc;
            best_weights = candidate_weights;
            println!("Iter {:4}: New Best MCC: {:.4} | FP: {:3} | FN: {:3} | Weights: {:?}", 
                iteration, best_mcc, fp as usize, fn_ as usize, best_weights);
        }
    }
}

fn convert_record(jr: JsonRecord) -> Record {
    let mut v = [0.0f32; 16];
    let src = &jr.vector;

    // 0. ln(1 + amount)
    v[0] = (1.0 + src[0] * 10000.0).ln() / (1.0f32 + 10000.0f32).ln();
    // 1. installments
    v[1] = src[1];
    // 2. amount_vs_avg_ratio
    v[2] = src[2];
    
    // 3. hour_sin & 4. hour_cos
    let hour = src[3] * 23.0;
    let hour_rad = hour * 2.0 * std::f32::consts::PI / 24.0;
    v[3] = hour_rad.sin();
    v[4] = hour_rad.cos();
    
    // 5. day_sin & 6. day_cos
    let dow = src[4] * 6.0;
    let day_rad = dow * 2.0 * std::f32::consts::PI / 7.0;
    v[5] = day_rad.sin();
    v[6] = day_rad.cos();
    
    // 7. ln(1 + minutes_since_last_tx)
    if src[5] >= 0.0 {
        v[7] = (1.0 + src[5] * 1440.0).ln() / (1.0f32 + 1440.0f32).ln();
    } else {
        v[7] = -1.0;
    }
    
    // 8. km_from_last_tx
    v[8] = src[6];
    // 9. km_from_home
    v[9] = src[7];
    // 10. tx_count_24h
    v[10] = src[8];

    // 11. Packed Binary (is_online:9, card_present:10, unknown_merchant:11)
    let mut packed_binary = 0.0f32;
    if src[9] > 0.5 { packed_binary += 1.0; }
    if src[10] > 0.5 { packed_binary += 2.0; }
    if src[11] > 0.5 { packed_binary += 4.0; }
    v[11] = packed_binary / 7.0;

    // 12. mcc_risk
    v[12] = src[12];
    
    // 13. merchant_avg_amount
    v[13] = src[13];

    v[14] = 0.0; // Padding
    
    // 15. Packed ID (31 bits) & Label (1 bit)
    // We'll set the ID in the main loop
    v[15] = if jr.label == "fraud" { 1.0 } else { 0.0 };
    
    Record { vector: v }
}

fn manhattan_distance_weighted(v1: &[f32; 16], v2: &[f32; 16]) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 { 
        sum += (v1[i] - v2[i]).abs() * FEATURE_WEIGHTS[i]; 
    }
    sum
}
