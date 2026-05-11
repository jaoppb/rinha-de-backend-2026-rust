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
    pub vector: [f32; 14], // 56 bytes
    pub label: u8,         // 1 byte
    pub _padding: [u8; 7], // 7 bytes
}

const N_CENTROIDS: usize = 256;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: indexer <input_file> [output_dir]");
        std::process::exit(1);
    }
    let input_path = &args[1];
    let output_dir = args.get(2).map(|s| s.as_str()).unwrap_or(".");

    println!("Opening input file: {}", input_path);
    let mut file = File::open(input_path)?;
    
    // Check if it's a gzip file by looking at the magic bytes
    let mut magic = [0u8; 2];
    let is_gzip = if file.read_exact(&mut magic).is_ok() {
        magic == [0x1f, 0x8b]
    } else {
        false
    };
    
    // Reset file pointer
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
    println!("Created dataset.bin at {:?}", dataset_path);

    // 2. Build IVF Index
    println!("Building IVF index with {} centroids...", N_CENTROIDS);
    
    let mut centroids = [[0.0f32; 14]; N_CENTROIDS];
    let stride = records.len() / N_CENTROIDS;
    for i in 0..N_CENTROIDS {
        centroids[i] = records[i * stride].vector;
    }

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

    // 3. Write IVF Index artifacts
    let c_path = Path::new(output_dir).join("centroids.bin");
    let mut f = File::create(&c_path)?;
    for c in &centroids {
        let bytes = unsafe {
            std::slice::from_raw_parts(c.as_ptr() as *const u8, std::mem::size_of::<[f32; 14]>())
        };
        f.write_all(bytes)?;
    }

    let i_path = Path::new(output_dir).join("indices.bin");
    let mut f = File::create(&i_path)?;
    let bytes = unsafe {
        std::slice::from_raw_parts(indices.as_ptr() as *const u8, indices.len() * 4)
    };
    f.write_all(bytes)?;

    let o_path = Path::new(output_dir).join("offsets.bin");
    let mut f = File::create(&o_path)?;
    let bytes = unsafe {
        std::slice::from_raw_parts(offsets.as_ptr() as *const u8, offsets.len() * 4)
    };
    f.write_all(bytes)?;

    println!("IVF index artifacts created: centroids.bin, indices.bin, offsets.bin");
    Ok(())
}

fn convert_record(jr: JsonRecord) -> Record {
    Record {
        vector: jr.vector,
        label: if jr.label == "fraud" { 1 } else { 0 },
        _padding: [0; 7],
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
