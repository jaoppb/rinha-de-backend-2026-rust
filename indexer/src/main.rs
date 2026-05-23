use flate2::read::GzDecoder;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

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

const N_CENTROIDS: usize = 4096;
const N_ITERATIONS: usize = 10;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: indexer <input_file> <output_dir>");
        std::process::exit(1);
    }
    let input_path = &args[1];
    let output_dir = &args[2];

    println!("Opening input file: {}", input_path);
    let mut file = File::open(input_path)?;
    
    // Detect GZIP
    let mut magic = [0u8; 2];
    let is_gzip = if file.read_exact(&mut magic).is_ok() {
        magic == [0x1f, 0x8b]
    } else {
        false
    };
    
    let file = File::open(input_path)?;
    let records: Vec<Record> = if is_gzip {
        println!("Detected GZIP format");
        let decoder = GzDecoder::new(file);
        let reader = BufReader::new(decoder);
        let json_records: Vec<JsonRecord> = serde_json::from_reader(reader)?;
        json_records.into_iter().map(convert_record).collect()
    } else {
        println!("Detected plain JSON format");
        let reader = BufReader::new(file);
        let json_records: Vec<JsonRecord> = serde_json::from_reader(reader)?;
        json_records.into_iter().map(convert_record).collect()
    };

    println!("Successfully loaded {} records", records.len());
    std::fs::create_dir_all(output_dir)?;

    // 1. K-Means
    println!("Clustering with K-Means ({} centroids, {} iterations)...", N_CENTROIDS, N_ITERATIONS);
    let mut centroids = vec![[0.0f32; 16]; N_CENTROIDS];
    let stride = (records.len() / N_CENTROIDS).max(1);
    for i in 0..N_CENTROIDS {
        let idx = (i * stride).min(records.len() - 1);
        centroids[i] = records[idx].vector;
    }

    let mut record_assignments = vec![0usize; records.len()];
    for iter in 0..N_ITERATIONS {
        println!("Iteration {}...", iter + 1);
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
            record_assignments[i] = best_c;
        }

        let mut new_centroids = vec![[0.0f32; 16]; N_CENTROIDS];
        let mut counts = vec![0usize; N_CENTROIDS];
        for (i, &assignment) in record_assignments.iter().enumerate() {
            for j in 0..14 {
                new_centroids[assignment][j] += records[i].vector[j];
            }
            counts[assignment] += 1;
        }
        for i in 0..N_CENTROIDS {
            if counts[i] > 0 {
                for j in 0..14 {
                    new_centroids[i][j] /= counts[i] as f32;
                }
                centroids[i] = new_centroids[i];
            }
        }
    }

    // 2. Write artifacts
    println!("Writing artifacts...");
    
    // Sort records by cluster
    let mut clustered_records = records.clone();
    let mut indexed_assignments: Vec<(usize, usize)> = record_assignments.iter().cloned().enumerate().map(|(i, c)| (c, i)).collect();
    indexed_assignments.sort_by_key(|&(_, i)| i); // Keep original order for stability? No, sort by cluster.
    indexed_assignments.sort_by_key(|&(c, _)| c);
    
    let mut offsets = vec![0u32; N_CENTROIDS + 1];
    let mut current_offset = 0;
    let mut final_records = Vec::with_capacity(records.len());
    
    let mut current_c = 0;
    offsets[0] = 0;
    for (c, i) in indexed_assignments {
        while current_c < c {
            current_c += 1;
            offsets[current_c] = current_offset;
        }
        final_records.push(records[i]);
        current_offset += 1;
    }
    while current_c < N_CENTROIDS {
        current_c += 1;
        offsets[current_c] = current_offset;
    }

    let dataset_path = Path::new(output_dir).join("dataset.bin");
    let mut f = File::create(&dataset_path)?;
    for record in &final_records {
        let bytes = unsafe { std::slice::from_raw_parts((record as *const Record) as *const u8, 64) };
        f.write_all(bytes)?;
    }

    let centroids_path = Path::new(output_dir).join("centroids.bin");
    let mut f = File::create(&centroids_path)?;
    for c in &centroids {
        let bytes = unsafe { std::slice::from_raw_parts(c.as_ptr() as *const u8, 64) };
        f.write_all(bytes)?;
    }

    let offsets_path = Path::new(output_dir).join("offsets.bin");
    let mut f = File::create(&offsets_path)?;
    for &o in &offsets {
        f.write_all(&o.to_ne_bytes())?;
    }

    println!("All artifacts created in {}", output_dir);
    Ok(())
}

fn convert_record(jr: JsonRecord) -> Record {
    let mut v = [0.0f32; 16];
    for i in 0..14 { v[i] = jr.vector[i]; }
    v[14] = 0.0;
    v[15] = if jr.label == "fraud" { 1.0 } else { 0.0 };
    Record { vector: v }
}

fn manhattan_distance(v1: &[f32; 16], v2: &[f32; 16]) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 { sum += (v1[i] - v2[i]).abs(); }
    sum
}
