# Data Architecture

This document describes the integrated indexing and data loading strategy used for the Rinha de Backend 2026.

## Overview

The dataset (`references.json.gz`) is processed during the **Docker build phase** by a dedicated Rust `indexer`. This tool converts raw JSON records into optimized binary formats (`.bin`) and builds an IVF (Inverted File) index. These binary artifacts are baked directly into the container image, eliminating the need for a separate data-loader service or shared `tmpfs` volumes for the dataset.

## Binary Format (Flat Records)

Each record in `dataset.bin` is exactly **64 bytes** long. This size matches the standard CPU cache line size, maximizing L1/L2 cache efficiency during KNN searches.

| Offset (Bytes) | Size (Bytes) | Type      | Description                          |
| -------------- | ------------ | --------- | ------------------------------------ |
| 0              | 56           | `[f32; 14]` | Vector features (Little Endian)      |
| 56             | 1            | `u8`      | Label (`0 = legit`, `1 = fraud`)     |
| 57             | 7            | Padding   | Zero-filled padding for 64B alignment |

## IVF Index Artifacts

The indexer generates four primary files in `/app/data/`:
1.  **`dataset.bin`**: The raw record data (approx. 183 MB for the full dataset).
2.  **`centroids.bin`**: 256 cluster centroids (14x f32 each).
3.  **`indices.bin`**: Mapping of cluster assignments to dataset indices.
4.  **`offsets.bin`**: Start/end positions for each cluster in the indices array.

The centroids are refined with a few deterministic k-means passes before the final
cluster assignments are written.

## Data Loading Strategy

The Rust API uses the `memmap2` crate to map these files from the local container filesystem at startup.

### Zero-Copy Access
By using `mmap`, the OS maps the file directly into the process's virtual memory space.
- **Memory Efficiency**: The 183MB dataset does not consume the API's heap.
- **Shared Page Cache**: Because multiple API instances share the same underlying Docker layers on the host, the Linux kernel naturally shares the page cache for these files across containers.

## Fraud Scoring

The API evaluates each request by:
1. Vectorizing the transaction into the same 14-feature space as the dataset.
2. Ranking IVF centroids with **Manhattan distance (L1 norm)**.
3. Inspecting the nearest 10 clusters and taking a distance-weighted vote from the 7 nearest records.

The project uses Manhattan distance instead of Euclidean distance as it provides a significant performance boost during the search phase while remaining permitted by the competition's detection rules. This keeps the scoring logic aligned with the index builder while making close neighbors count more than distant ones.

### Health Checks
The API is considered "ready" only after all four binary files have been successfully mapped and the IVF index is initialized. HAProxy monitors the `/ready` endpoint to ensure traffic is only routed to fully initialized instances.

## Build Process

1.  **Indexer Build**: The `indexer` project is compiled.
2.  **Data Generation**: The `indexer` runs against the input JSON, producing the `.bin` files in `/app/data`.
3.  **API Build**: The `api` project is compiled.
4.  **Image Assembly**: The final slim image copies the `api` binary and the `/app/data` artifacts.

This self-contained architecture ensures consistent performance, simplified deployment, and zero external dependencies at runtime.
