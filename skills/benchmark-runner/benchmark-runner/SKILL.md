---
name: benchmark-runner
description: Automates the full benchmarking workflow for the Rinha de Backend 2026 project, including verbose compilation, environment startup, performance testing, and data collection. Use when requested to "run benchmarks", "collect performance data", or "test with verbose logging".
---

# Benchmark Runner

This skill automates the execution and data collection of performance benchmarks, running both standard and verbose configurations sequentially to compare results.

## Workflow

### Phase 1: Non-Verbose Run (Baseline)
1. **Build**: Execute `make build-release` to compile the API and Load Balancer with the full dataset and default logging.
2. **Start Environment**: Execute `make up` to start the containers in detached mode.
3. **Wait for Readiness**: Wait for the "Successfully loaded all datasets" message in the API logs before proceeding.
4. **Execute Tests**: Execute `make test` to run the k6 performance test suite.
5. **Collect Phase 1 Data**:
    - Capture the full output of the `make test` command.
    - Retrieve and summarize logs from `api1`, `api2`, and `lb` using `docker-compose logs --tail=500`.
6. **Cleanup**: Execute `make down` to stop and remove containers.

### Phase 2: Verbose Run (Profiling)
1. **Build with Verbose Logging**: Execute `make build-release-verbose` to compile with the `verbose-logging` feature enabled.
2. **Start Environment**: Execute `make up` to start the containers in detached mode.
3. **Wait for Readiness**: Wait for the "Successfully loaded all datasets" message in the API logs.
4. **Execute Tests**: Execute `make test` to run the k6 performance test suite.
5. **Collect Phase 2 Data**:
    - Capture the full output of the `make test` command.
    - Retrieve and summarize logs from `api1`, `api2`, and `lb` using `docker-compose logs --tail=500`.
6. **Cleanup**: Execute `make down` to stop and remove containers.

## Data Collection Pattern

When collecting data, prioritize and **compare** the following metrics between the two runs:
- `http_req_duration`: p99 and avg (look for overhead introduced by verbose logging).
- `iterations`: total count and rate (throughput comparison).
- `dropped_iterations`: to identify if the target rate was met in both scenarios.
- `error_count` and specific error messages from k6 or service logs.
- Identify any bottlenecks or patterns that appear only in the verbose logs.
