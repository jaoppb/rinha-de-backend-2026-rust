# rinha-de-backend-2026

High-performance Rust backend entry for the Rinha de Backend 2026 competition.
This project is optimized for extreme throughput and low latency under tight
resource constraints (0.3-0.4 CPU per container).

## Architecture Overview

The system uses a highly specialized architecture to minimize overhead:

- **Asynchronous I/O:** Powered by `epoll` (via `mio`) for all network and file
  operations. Note: `io_uring` was the original target but is currently disabled
  due to environment-specific security constraints.
- **FD-Level Load Balancing:** The custom load balancer (`lb/`) accepts client
  connections and hands off the file descriptors to API workers via Unix Domain
  Sockets using `SCM_RIGHTS`. This avoids proxying overhead.
- **Zero-Copy Data Access:** The `indexer/` pre-processes JSON data into 64-byte
  aligned binary records. The `api/` uses `mmap` to access these records
  directly from the OS page cache.
- **Custom Parsers:** To eliminate generic library overhead, the project uses
  hand-rolled HTTP (`api/src/http_parser.rs`) and JSON
  (`api/src/json_parser.rs`) parsers.
- **Search Optimization:** KNN search for fraud detection is implemented using
  an Inverted File (IVF) index with Manhattan distance, optimized for cache
  efficiency (`api/src/knn.rs`).

## Performance Targets

- **p99 Latency:** < 0.5 ms
- **HTTP Success Rate:** 100% (0% failures)
- **Classification Accuracy:** 100% (0% misclassifications)

## Performance Baselines

### 2026-05-22 Benchmark (Optimized)

- **Throughput:** 450.48 req/s
- **p99 Latency:** 2.68 ms
- **Success Rate:** 100%
- **KNN Tail Latency:** 4.97 ms (Reduced from 17.6ms)
- **Status:** KNN loop unrolled (4x) and prefetched. Tail latency target (<5ms)
  achieved.

### 2026-05-22 Benchmark (Baseline)

- **Throughput:** 450.48 req/s
- **p99 Latency:** 2.02 ms
- **Success Rate:** 100%
- **KNN Tail Latency:** 17.6 ms

### 2026-05-21 Benchmark

- **Throughput:** 450.05 req/s
- **p99 Latency:** 268.66 ms
- **Success Rate:** 43.62%
- **Primary Bottleneck:** API event loop blocking on synchronous KNN searches,
  causing UDS buffer overflows (503 errors).

## Key Components

- `api/`: The main worker process. It receives connections from the LB, parses
  requests, performs KNN searches, and serves responses.
  - `build.rs`: Bakes static lookup tables from `resources/*.json` directly into
    the binary.
- `lb/`: The load balancer. It manages the frontend socket and distributes load
  to API instances.
- `indexer/`: A pre-processing tool that converts raw data into optimized binary
  artifacts.
- `resources/`: Contains the raw JSON data and normalization factors.

## Tech Stack

- **Language:** Rust (Stable/Nightly depending on `io_uring` features).
- **Infrastructure:** Docker, Docker Compose.
- **I/O Interface:** `io_uring`.
- **Data Strategy:** Binary artifacts, `mmap`.

## Development Workflows

### Building & Running

The project is managed via a `Makefile` and `docker-compose.yml`.

- **Build all:** `make build`
- **Run local stack:** `docker compose up`
- **Prepare data:** The indexer must run before the API can start with fresh
  data.

### Testing

Testing is primarily done via `k6` scripts in the `test/` directory.

- `test/smoke.js`: Quick verification of basic functionality.
- `test/test.js`: Full performance/load test.

## Implementation Details to Note

- **No standard async:** Avoid adding `tokio` or other heavy async runtimes
  unless absolutely necessary. Stick to the `io_uring` event loop.
- **Memory Alignment:** Binary records are 64-byte aligned. Ensure any changes
  to the data format maintain this for cache efficiency.
- **Static Lookups:** Many configuration files are baked at compile-time. If you
  update files in `resources/`, you must recompile the API.
- **Load Balancing:** Strict round-robin distribution to upstream API workers is
  a hard requirement. Do not implement least-connections or other dynamic
  routing algorithms.
- **Target Hardware:** The competition/benchmark environment is a Mac mini
  (Late 2014) with a 2.6GHz dual-core Intel Core i5 processor (Haswell
  microarchitecture) and 8GB RAM (<https://support.apple.com/en-us/111931>). This
  means AVX2 is the maximum supported vector extension; AVX-512 is not
  available.

## Documentation

- `docs/DATA_ARCHITECTURE.md`: Detailed explanation of the binary record format
  and memory-mapping strategy.
- `info.json`: Project and participant metadata.
