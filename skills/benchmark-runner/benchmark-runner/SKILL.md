---
name: benchmark-runner
description: Automates the full benchmarking workflow for the Rinha de Backend 2026 project, including verbose compilation, environment startup, performance testing, and data collection. Use when requested to "run benchmarks", "collect performance data", or "test with verbose logging".
---

# Benchmark Runner

This skill automates the execution and data collection of performance benchmarks using the automated monitor script.

## Workflow

1. **Build with Verbose Logging**: Execute `make build-release-verbose` to compile the API and Load Balancer with the `verbose-logging` feature and full dataset.
2. **Run Monitor**: Execute `make monitor` to start the environment, wait for readiness, run tests, and collect statistics.
3. **Collect Data**:
    - Capture the full output of the `make monitor` command.
    - Review `test_stats.log` for granular operation-level timing statistics.
    - Focus on latency metrics (p99) from k6 output and `avg_us` per operation from `test_stats.log`.

## Data Collection Pattern

When collecting data, prioritize the following metrics:
- `http_req_duration`: p99 and avg.
- `iterations`: total count and rate.
- `dropped_iterations`: to identify if the target rate was met.
- **Timing Statistics** (from `test_stats.log`): `avg_us`, `min_us`, `max_us` per operation (e.g., `knn`, `http_parse`, `json_parse`).
- `error_count` and specific error messages from k6 or service logs.
