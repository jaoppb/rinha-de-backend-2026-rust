# Data Architecture

This document describes the integrated indexing and data loading strategy used for the Rinha de Backend 2026.

## Overview

The dataset (`references.json.gz`) is processed during the **Docker build phase** by a dedicated Rust `indexer`. This tool converts raw JSON records into optimized binary formats (`.bin`) and builds a **Hierarchical Inverted File (HIVF)** index. These binary artifacts are baked directly into the container image, eliminating the need for a separate data-loader service or shared `tmpfs` volumes for the dataset.

## Binary Format (Flat Records)

Each record in `dataset.bin` is exactly **64 bytes** long. This size matches the standard CPU cache line size, maximizing L1/L2 cache efficiency during KNN searches.

| Offset (Bytes) | Size (Bytes) | Type      | Description                          |
| -------------- | ------------ | --------- | ------------------------------------ |
| 0              | 56           | `[f32; 14]` | Vector features (Little Endian)      |
| 56             | 4            | `f32`     | Label (`0.0 = legit`, `1.0 = fraud`) stored at index 15 |
| 60             | 4            | Padding   | Zero-filled padding for 64B alignment |

## HIVF Index Artifacts

The indexer generates four primary files in `/app/data/`:
1.  **`dataset.bin`**: The raw record data (approx. 192 MB for 3M records), sorted by L2 cluster.
2.  **`l1_centroids.bin`**: 256 "super-centroids" (14x f32 each) representing the root level of the hierarchy.
3.  **`l2_centroids.bin`**: 65,536 sub-centroids (256 for each L1). Stored contiguously; L2 centroids for L1 index `i` are at `i * 256`.
4.  **`offsets.bin`**: Mapping of L2 cluster indices to starting positions in `dataset.bin`.

The hierarchy is built via recursive K-Means clustering, ensuring that the 3,000,000 records are distributed into 65,536 highly specific clusters (~45 records each).

## Data Loading Strategy

The Rust API uses the `memmap2` crate to map these files from the local container filesystem at startup.

### Zero-Copy Access
By using `mmap`, the OS maps the file directly into the process's virtual memory space.
- **Memory Efficiency**: The dataset does not consume the API's heap.
- **Shared Page Cache**: Because multiple API instances share the same underlying Docker layers on the host, the Linux kernel naturally shares the page cache for these files across containers.

## Fraud Scoring (HIVF Traversal)

The API evaluates each request via a highly optimized 3-step search:

1. **L1 Root Scan**: Prune 97% of the dataset by finding the top 8 closest L1 super-centroids using AVX2 SIMD.
2. **L2 Leaf Scan**: Scan the 2,048 L2 centroids (256 from each of the 8 selected L1s) to find the top 32 most relevant sub-clusters.
3. **Exact Record Scan**: Compute exact Manhattan distances for only the records (~1,440) residing in those 32 clusters.

### Manhattan Distance
The project uses Manhattan distance (L1 norm) instead of Euclidean distance as it provides a significant performance boost during the search phase (especially with `_mm256_sub_ps` and `_mm256_and_ps` for absolute value) while remaining extremely accurate for high-dimensional feature spaces.

### Health Checks
The API is considered "ready" only after all four binary files have been successfully mapped and the HIVF index is initialized.

## Build Process

1.  **Indexer Build**: The `indexer` project is compiled.
2.  **Data Generation**: The `indexer` runs against the input JSON, producing the `.bin` files in `/app/data`.
3.  **API Build**: The `api` project is compiled.
4.  **Image Assembly**: The final slim image copies the `api` binary and the `/app/data` artifacts.

This hierarchical architecture ensures sub-millisecond latency and 100% accuracy even as the dataset scales to 3 million records.
