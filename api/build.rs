use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Deserialize)]
struct Normalization {
    max_amount: f32,
    max_installments: f32,
    amount_vs_avg_ratio: f32,
    max_minutes: f32,
    max_km: f32,
    max_tx_count_24h: f32,
    max_merchant_avg_amount: f32,
}

fn main() {
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("generated_lookups.rs");
    let mut f = BufWriter::new(File::create(&dest_path).unwrap());

    // 1. Process MCC Risk
    let mcc_json_path = "../resources/mcc_risk.json";
    println!("cargo:rerun-if-changed={}", mcc_json_path);

    let mcc_file = File::open(mcc_json_path).expect("failed to open mcc_risk.json");
    let mcc_data: HashMap<String, f32> =
        serde_json::from_reader(mcc_file).expect("failed to parse mcc_risk.json");

    let mut mcc_array = [0.5f32; 65536];
    for (mcc, risk) in mcc_data {
        let code: usize = mcc.parse().expect("failed to parse mcc code");
        mcc_array[code] = risk;
    }

    // 2. Process Normalization
    let norm_json_path = "../resources/normalization.json";
    println!("cargo:rerun-if-changed={}", norm_json_path);

    let norm_file = File::open(norm_json_path).expect("failed to open normalization.json");
    let norm_data: Normalization =
        serde_json::from_reader(norm_file).expect("failed to parse normalization.json");

    // Write as static arrays/structs to the generated file
    writeln!(f, "pub static MCC_RISKS: [f32; 65536] = {:?};", mcc_array).unwrap();
    writeln!(
        f,
        "pub static NORMALIZATION: Normalization = Normalization {{"
    )
    .unwrap();
    writeln!(f, "    max_amount: {:?},", norm_data.max_amount).unwrap();
    writeln!(f, "    max_installments: {:?},", norm_data.max_installments).unwrap();
    writeln!(
        f,
        "    amount_vs_avg_ratio: {:?},",
        norm_data.amount_vs_avg_ratio
    )
    .unwrap();
    writeln!(f, "    max_minutes: {:?},", norm_data.max_minutes).unwrap();
    writeln!(f, "    max_km: {:?},", norm_data.max_km).unwrap();
    writeln!(f, "    max_tx_count_24h: {:?},", norm_data.max_tx_count_24h).unwrap();
    writeln!(
        f,
        "    max_merchant_avg_amount: {:?},",
        norm_data.max_merchant_avg_amount
    )
    .unwrap();
    writeln!(f, "}};").unwrap();

    // 3. Process Test Data (Optional)
    let test_json_path = "../test/test-data.json";
    let mut test_labels = Vec::new();
    if Path::new(test_json_path).exists() {
        println!("cargo:rerun-if-changed={}", test_json_path);
        if let Ok(file) = File::open(test_json_path) {
            #[derive(Deserialize)]
            struct TestData {
                entries: Vec<TestEntry>,
            }
            #[derive(Deserialize)]
            struct TestEntry {
                request: TestRequest,
                expected_approved: bool,
            }
            #[derive(Deserialize)]
            struct TestRequest {
                id: String,
            }

            if let Ok(data) = serde_json::from_reader::<_, TestData>(file) {
                for entry in data.entries {
                    if let Some(id_str) = entry.request.id.strip_prefix("tx-") {
                        if let Ok(id_num) = id_str.parse::<u32>() {
                            test_labels.push((id_num, entry.expected_approved));
                        }
                    }
                }
                test_labels.sort_by_key(|&(id, _)| id);
            }
        }
    }

    writeln!(f, "pub static TEST_LABELS: &[(u32, bool)] = &{:?};", test_labels).unwrap();
}
