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
    pub vector: [f32; 16],  // 64 bytes
    pub label: u8,          // 1 byte
    pub _padding: [u8; 63], // 63 bytes -> Total 128 bytes (aligned 64)
}

const N_CENTROIDS: usize = 256;
const N_ITERATIONS: usize = 5;

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

    // 1. Write dataset.bin
    let dataset_path = Path::new(output_dir).join("dataset.bin");
    let mut f = File::create(&dataset_path)?;
    for record in &records {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (record as *const Record) as *const u8,
                std::mem::size_of::<Record>(),
            )
        };
        f.write_all(bytes)?;
    }

    // 2. Build IVF Index
    let mut centroids = [[0.0f32; 16]; N_CENTROIDS];
    let stride = (records.len() / N_CENTROIDS).max(1);
    for i in 0..N_CENTROIDS {
        let idx = (i * stride).min(records.len() - 1);
        centroids[i] = records[idx].vector;
    }

    let mut assignments = vec![0u16; records.len()];
    for _ in 0..N_ITERATIONS {
        let mut counts = [0u32; N_CENTROIDS];
        let mut sums = [[0.0f32; 16]; N_CENTROIDS];

        for (i, record) in records.iter().enumerate() {
            let mut min_dist = f32::MAX;
            let mut best_c = 0usize;
            for (c_idx, centroid) in centroids.iter().enumerate() {
                let dist = manhattan_distance(&record.vector, centroid);
                if dist < min_dist {
                    min_dist = dist;
                    best_c = c_idx;
                }
            }
            assignments[i] = best_c as u16;
            counts[best_c] += 1;
            for d in 0..16 {
                sums[best_c][d] += record.vector[d];
            }
        }

        for i in 0..N_CENTROIDS {
            if counts[i] > 0 {
                let inv = 1.0 / counts[i] as f32;
                for d in 0..16 {
                    centroids[i][d] = sums[i][d] * inv;
                }
            } else {
                let idx = (i * stride).min(records.len() - 1);
                centroids[i] = records[idx].vector;
            }
        }
    }

    let mut counts = [0u32; N_CENTROIDS];
    for &c_idx in &assignments {
        counts[c_idx as usize] += 1;
    }

    let mut offsets = [0u32; N_CENTROIDS + 1];
    for i in 0..N_CENTROIDS {
        offsets[i + 1] = offsets[i] + counts[i];
    }

    let mut indices = vec![0u32; records.len()];
    let mut current_pos = offsets;
    for (i, &c_idx) in assignments.iter().enumerate() {
        let pos = current_pos[c_idx as usize];
        indices[pos as usize] = i as u32;
        current_pos[c_idx as usize] += 1;
    }

    // 3. Write Index Artifacts
    File::create(Path::new(output_dir).join("centroids.bin"))?
        .write_all(unsafe { std::slice::from_raw_parts(centroids.as_ptr() as *const u8, std::mem::size_of_val(&centroids)) })?;

    File::create(Path::new(output_dir).join("indices.bin"))?
        .write_all(unsafe { std::slice::from_raw_parts(indices.as_ptr() as *const u8, indices.len() * 4) })?;

    File::create(Path::new(output_dir).join("offsets.bin"))?
        .write_all(unsafe { std::slice::from_raw_parts(offsets.as_ptr() as *const u8, offsets.len() * 4) })?;

    println!("All artifacts created in {}", output_dir);
    Ok(())
}

fn convert_record(jr: JsonRecord) -> Record {
    let mut v = [0.0f32; 16];
    v[..14].copy_from_slice(&jr.vector);
    Record {
        vector: v,
        label: if jr.label == "fraud" { 1 } else { 0 },
        _padding: [0; 63],
    }
}

fn manhattan_distance(v1: &[f32; 16], v2: &[f32; 16]) -> f32 {
    let mut sum = 0.0;
    for i in 0..16 {
        sum += (v1[i] - v2[i]).abs();
    }
    sum
}
