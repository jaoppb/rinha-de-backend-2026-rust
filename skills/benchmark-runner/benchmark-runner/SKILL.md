---
name: benchmark-runner
description: Automates the full benchmarking workflow for the Rinha de Backend 2026 project, including verbose compilation, environment startup, performance testing, and data collection. Use when requested to "run benchmarks", "collect performance data", or "test with verbose logging".
---

# Benchmark Runner

This skill automates the execution and data collection of performance benchmarks using the automated monitor script.

## Workflow

1. **Baseline Run (Non-Verbose)**: Execute `make build-release && make monitor` to measure raw performance without logging overhead.
2. **Analysis Run (Verbose)**: Execute `make build-release-verbose && make monitor` to collect granular operation-level timing statistics.
3. **Automated Suite**: Alternatively, use `scripts/benchmark_suite.sh` to run both sequentially and aggregate results.
4. **Collect Data**:
    - Capture the full output of the `make monitor` command or `benchmark_suite.sh`.
    - Review `test_stats.log` (generated in verbose mode) for operation-level timing.
    - Focus on latency metrics (p99) from k6 output and `avg_us` per operation.

## Data Collection Pattern

When collecting data, prioritize the following metrics:
- `http_req_duration`: p99 and avg.
- `iterations`: total count and rate.
- `dropped_iterations`: to identify if the target rate was met.
- **Timing Statistics** (from `test_stats.log`): `avg_us`, `min_us`, `max_us` per operation (e.g., `knn`, `http_parse`, `json_parse`).
- `error_count` and specific error messages from k6 or service logs.
