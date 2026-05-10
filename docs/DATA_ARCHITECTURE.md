# Data Architecture

This document describes the optimized data loading and storage strategy used for the Rinha de Backend 2026.

## Overview

The dataset (`references.json.gz`) is converted from a compressed JSON format into a high-performance, cache-line aligned binary format during the Docker build phase. This binary data is then served via shared memory (`/dev/shm`) to the Rust API instances.

## Binary Format (Flat Records)

Each record in `dataset.bin` is exactly **64 bytes** long. This size is chosen to match the standard CPU cache line size, ensuring that a single memory fetch retrieves exactly one record, maximizing L1/L2 cache efficiency during KNN searches.

### Memory Layout per Record

| Offset (Bytes) | Size (Bytes) | Type      | Description                          |
| -------------- | ------------ | --------- | ------------------------------------ |
| 0              | 56           | `[f32; 14]` | Vector features (Little Endian)      |
| 56             | 1            | `u8`      | Label (`0 = legit`, `1 = fraud`)     |
| 57             | 7            | Padding   | Zero-filled padding for 64B alignment |

### Total Size
The final main dataset is approximately **183.1 MB**, stored as a contiguous array of these 64-byte structures.

## Auxiliary Data

In addition to the main vector dataset, two auxiliary files are converted to binary to eliminate runtime JSON parsing.

### MCC Risk (`mcc_risk.bin`)

Stored as a flat list of `(u16, f32)` pairs.
- **`u16`**: MCC code (2 bytes)
- **`f32`**: Risk factor (4 bytes)
- **Total Record Size**: 6 bytes

### Normalization Constants (`normalization.bin`)

Stored as a single block of 7 `f32` values (28 bytes). The values are stored in the following fixed order:
1. `max_amount`
2. `max_installments`
3. `amount_vs_avg_ratio`
4. `max_minutes`
5. `max_km`
6. `max_tx_count_24h`
7. `max_merchant_avg_amount`

## Data Loader Service

The `data-loader` service (defined in `docker-compose.yml`) performs the following:

1.  **Conversion:** Runs `data/convert.py` during `docker build`.
2.  **Shared Memory:** At runtime, it copies the `.bin` files to `/dev/shm/`.
3.  **IPC:** It is configured with `ipc: shareable`, allowing other containers to attach to its IPC namespace.

**Note:** The `data-loader` service must be configured with `shm_size: '184mb'` in `docker-compose.yml`. The default Docker `/dev/shm` size (64MB) is insufficient for the 183.1MB dataset.

## Accessing from Rust API

The Rust API containers are configured with `ipc: service:data-loader`. This allows them to access the same `/dev/shm` space.

### Recommended Integration Strategy

To achieve zero-copy performance, the Rust API should use the `memmap2` crate. Below are the recommended struct definitions and mapping logic.

#### Struct Definitions

```rust
#[repr(C, align(64))]
pub struct Record {
    pub vector: [f32; 14], // 56 bytes
    pub label: u8,         // 1 byte
    _padding: [u8; 7],     // 7 bytes
}

#[repr(C)]
pub struct MccRisk {
    pub mcc: u16,
    pub risk: f32,
}

#[repr(C)]
pub struct Normalization {
    pub max_amount: f32,
    pub max_installments: f32,
    pub amount_vs_avg_ratio: f32,
    pub max_minutes: f32,
    pub max_km: f32,
    pub max_tx_count_24h: f32,
    pub max_merchant_avg_amount: f32,
}
```

#### Mapping Logic

```rust
use memmap2::Mmap;
use std::fs::File;

// 1. Main Dataset
let file = File::open("/dev/shm/dataset.bin")?;
let mmap = unsafe { Mmap::map(&file)? };
let records: &[Record] = unsafe {
    std::slice::from_raw_parts(mmap.as_ptr() as *const Record, mmap.len() / 64)
};

// 2. MCC Risk (Small enough to read into a Map or keep as slice)
let mcc_file = File::open("/dev/shm/mcc_risk.bin")?;
let mcc_mmap = unsafe { Mmap::map(&mcc_file)? };
let mcc_risks: &[MccRisk] = unsafe {
    std::slice::from_raw_parts(mcc_mmap.as_ptr() as *const MccRisk, mcc_mmap.len() / 6)
};

// 3. Normalization Constants
let norm_bytes = std::fs::read("/dev/shm/normalization.bin")?;
let normalization: &Normalization = unsafe {
    &*(norm_bytes.as_ptr() as *const Normalization)
};
```

This approach allows the API to perform KNN searches directly on the shared memory without allocating 183MB on its own heap, staying well within the 160MB RAM limit.
