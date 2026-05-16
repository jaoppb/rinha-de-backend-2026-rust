use memmap2::Mmap;
use std::fs::File;

#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct Record {
    pub vector: [f32; 14], // 56 bytes
    pub label: u8,         // 1 byte
    pub _padding: [u8; 7], // 7 bytes
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
    _centroids_mmap: Mmap,
    _indices_mmap: Mmap,
    _offsets_mmap: Mmap,
    pub centroids: &'static [[f32; 14]],
    pub indices: &'static [u32],
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
    let centroids_file = File::open("/app/data/centroids.bin")?;
    let centroids_mmap = unsafe { Mmap::map(&centroids_file)? };

    let indices_file = File::open("/app/data/indices.bin")?;
    let indices_mmap = unsafe { Mmap::map(&indices_file)? };

    let offsets_file = File::open("/app/data/offsets.bin")?;
    let offsets_mmap = unsafe { Mmap::map(&offsets_file)? };

    let centroids = unsafe {
        std::slice::from_raw_parts(
            centroids_mmap.as_ptr() as *const [f32; 14],
            centroids_mmap.len() / std::mem::size_of::<[f32; 14]>(),
        )
    };

    let indices = unsafe {
        std::slice::from_raw_parts(indices_mmap.as_ptr() as *const u32, indices_mmap.len() / 4)
    };

    let offsets = unsafe {
        std::slice::from_raw_parts(offsets_mmap.as_ptr() as *const u32, offsets_mmap.len() / 4)
    };

    Ok(IvfData {
        _centroids_mmap: centroids_mmap,
        _indices_mmap: indices_mmap,
        _offsets_mmap: offsets_mmap,
        centroids: unsafe { std::mem::transmute(centroids) },
        indices: unsafe { std::mem::transmute(indices) },
        offsets: unsafe { std::mem::transmute(offsets) },
    })
}

pub fn load_lookups() -> LookupData {
    LookupData {
        mcc_risks: &MCC_RISKS,
        normalization: &NORMALIZATION,
    }
}
