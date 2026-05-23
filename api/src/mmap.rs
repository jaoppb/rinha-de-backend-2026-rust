use memmap2::Mmap;
use std::fs::File;

#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct Record {
    pub vector: [f32; 16],  // 64 bytes, label stored in vector[15]
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

// Include the generated data
include!(concat!(env!("OUT_DIR"), "/generated_lookups.rs"));

pub struct Dataset {
    _mmap: Mmap,
    pub records: &'static [Record],
}

pub struct IvfData {
    _l1_mmap: Mmap,
    _l2_mmap: Mmap,
    _offsets_mmap: Mmap,
    pub l1_centroids: &'static [[f32; 16]],
    pub l2_centroids: &'static [[f32; 16]],
    pub offsets: &'static [u32],
}

pub struct LookupData {
    pub mcc_risks: &'static [f32; 65536],
    pub normalization: &'static Normalization,
}

pub fn load_dataset() -> std::io::Result<Dataset> {
    let file = File::open("/app/data/dataset.bin")?;
    let mmap = unsafe { Mmap::map(&file)? };
    let ptr = mmap.as_ptr() as *const Record;
    let len = mmap.len() / std::mem::size_of::<Record>();

    let records = unsafe { std::slice::from_raw_parts(ptr, len) };
    let records_static = unsafe { std::mem::transmute::<&[Record], &'static [Record]>(records) };

    Ok(Dataset {
        _mmap: mmap,
        records: records_static,
    })
}

pub fn load_ivf_data() -> std::io::Result<IvfData> {
    let l1_file = File::open("/app/data/l1_centroids.bin")?;
    let l1_mmap = unsafe { Mmap::map(&l1_file)? };

    let l2_file = File::open("/app/data/l2_centroids.bin")?;
    let l2_mmap = unsafe { Mmap::map(&l2_file)? };

    let offsets_file = File::open("/app/data/offsets.bin")?;
    let offsets_mmap = unsafe { Mmap::map(&offsets_file)? };

    let l1_centroids = unsafe {
        std::slice::from_raw_parts(
            l1_mmap.as_ptr() as *const [f32; 16],
            l1_mmap.len() / 64,
        )
    };

    let l2_centroids = unsafe {
        std::slice::from_raw_parts(
            l2_mmap.as_ptr() as *const [f32; 16],
            l2_mmap.len() / 64,
        )
    };

    let offsets = unsafe {
        std::slice::from_raw_parts(offsets_mmap.as_ptr() as *const u32, offsets_mmap.len() / 4)
    };

    Ok(IvfData {
        _l1_mmap: l1_mmap,
        _l2_mmap: l2_mmap,
        _offsets_mmap: offsets_mmap,
        l1_centroids: unsafe { std::mem::transmute(l1_centroids) },
        l2_centroids: unsafe { std::mem::transmute(l2_centroids) },
        offsets: unsafe { std::mem::transmute(offsets) },
    })
}


pub fn load_lookups() -> LookupData {
    LookupData {
        mcc_risks: &MCC_RISKS,
        normalization: &NORMALIZATION,
    }
}
