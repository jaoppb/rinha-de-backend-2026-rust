---
name: benchmark-runner
description: Automates the full benchmarking workflow for the Rinha de Backend 2026 project, including verbose compilation, environment startup, performance testing, and data collection. Use when requested to "run benchmarks", "collect performance data", or "test with verbose logging".
---

# Benchmark Runner

This skill automates the execution and data collection of performance benchmarks.

## Workflow

1. **Build with Verbose Logging**: Execute `make build-release-verbose` to compile the API and Load Balancer with the `verbose-logging` feature and full dataset.
2. **Start Environment**: Execute `make up` to start the containers in detached mode.
3. **Wait for Readiness**: Wait for the "Successfully loaded all datasets" message in the API logs before proceeding.
4. **Execute Tests**: Execute `make test` to run the k6 performance test suite.
5. **Collect Data**:
    - Capture the full output of the `make test` command.
    - Retrieve and summarize logs from `api1`, `api2`, and `lb` using `docker-compose logs --tail=500`.
    - Focus on latency metrics (p99), throughput (iterations/s), and any "Request Failed" errors.

## Data Collection Pattern

When collecting data, prioritize the following metrics:
- `http_req_duration`: p99 and avg.
- `iterations`: total count and rate.
- `dropped_iterations`: to identify if the target rate was met.
- `error_count` and specific error messages from k6 or service logs.
