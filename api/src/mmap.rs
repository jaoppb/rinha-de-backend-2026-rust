use memmap2::Mmap;
use std::fs::File;

#[repr(C, align(64))]
pub struct Record {
    pub vector: [f32; 14], // 56 bytes
    pub label: u8,         // 1 byte
    pub _padding: [u8; 7], // 7 bytes
}

#[repr(C)]
pub struct MccRisk {
    pub mcc: u16,
    pub risk: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Normalization {
    pub max_amount: f32,
    pub max_installments: f32,
    pub amount_vs_avg_ratio: f32,
    pub max_minutes: f32,
    pub max_km: f32,
    pub max_tx_count_24h: f32,
    pub max_merchant_avg_amount: f32,
}

pub struct Dataset {
    _mmap: Mmap, // Kept to ensure the memory stays mapped
    pub records: &'static [Record],
}

pub struct LookupData {
    pub mcc_risks: [f32; 65536],
    pub normalization: Normalization,
}

pub fn load_dataset() -> std::io::Result<Dataset> {
    let file = File::open("/dev/shm/dataset.bin")?;
    let mmap = unsafe { Mmap::map(&file)? };
    let ptr = mmap.as_ptr() as *const Record;
    let len = mmap.len() / std::mem::size_of::<Record>();

    // Safety: The mmap lives for the duration of the Dataset struct.
    // We transmute to 'static because this dataset is global and intended
    // to live for the process duration.
    let records = unsafe { std::slice::from_raw_parts(ptr, len) };
    let records_static = unsafe { std::mem::transmute::<&[Record], &'static [Record]>(records) };

    Ok(Dataset {
        _mmap: mmap,
        records: records_static,
    })
}

pub fn load_lookups() -> std::io::Result<LookupData> {
    // Load MCC Risk
    let mut mcc_risks = [0.5f32; 65536]; // Default risk is 0.5
    let mcc_file = File::open("/dev/shm/mcc_risk.bin")?;
    let mcc_mmap = unsafe { Mmap::map(&mcc_file)? };

    let mcc_record_size = std::mem::size_of::<MccRisk>();
    let mcc_len = mcc_mmap.len() / mcc_record_size;
    let mcc_records: &[MccRisk] =
        unsafe { std::slice::from_raw_parts(mcc_mmap.as_ptr() as *const MccRisk, mcc_len) };

    for record in mcc_records {
        mcc_risks[record.mcc as usize] = record.risk;
    }

    // Load Normalization
    let norm_file = File::open("/dev/shm/normalization.bin")?;
    let norm_mmap = unsafe { Mmap::map(&norm_file)? };
    let normalization: Normalization =
        unsafe { std::ptr::read(norm_mmap.as_ptr() as *const Normalization) };

    Ok(LookupData {
        mcc_risks,
        normalization,
    })
}
