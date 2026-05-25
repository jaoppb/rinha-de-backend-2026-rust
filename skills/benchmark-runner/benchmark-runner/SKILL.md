---
name: benchmark-runner
description: Automates the full benchmarking workflow for the Rinha de Backend 2026 project, including verbose compilation, environment startup, performance testing, and data collection. Use when requested to "run benchmarks", "collect performance data", or "test with verbose logging".
---

# Benchmark Runner

This skill automates the execution and data collection of performance benchmarks using targets defined in the Makefile.

## Workflow

1. **Ask the User**: Before running any benchmarks, use the `ask_question` tool to ask the user which type of benchmark test to execute. Provide the following options based on the `Makefile` targets: `smoke`, `test`, `test-thermal`, `test-sustained`, `test-saturation`, and `test-spike`.
2. **Baseline Run (Non-Verbose)**: Execute `make build-release` followed by `make <selected-test>` (or run them together if appropriate) to measure raw performance without logging overhead.
3. **Analysis Run (Verbose)**: Execute `make build-release-verbose` followed by `make <selected-test>` to collect granular operation-level timing statistics.
4. **Collect Data**:
    - Capture the full output of the executed `make <selected-test>` command.
    - Review `test_stats.log` (generated in verbose mode) for operation-level timing.
    - Focus on latency metrics (p99) from k6 output and `avg_us` per operation.
5. **Wait for Model Switch**: Prompt the user to change the active model (e.g., from a fast execution model like Gemini 3.5 Flash back to a reasoning/analysis model like Gemini 3.5 Pro) and wait for the model change before entering Plan Mode or starting the planning phase.
6. **Plan Mode Transition**: Once the model has been switched, enter Plan Mode.
7. **Analysis & Suggestions**: Analyze the benchmark data and suggest concrete architectural or code-level improvements based on the findings.

## Data Collection Pattern

When collecting data, prioritize the following metrics:
- `http_req_duration`: p99 and avg.
- `iterations`: total count and rate.
- `dropped_iterations`: to identify if the target rate was met.
- **Timing Statistics** (from `test_stats.log`): `avg_us`, `min_us`, `max_us` per operation (e.g., `knn`, `http_parse`, `json_parse`).
- `error_count` and specific error messages from k6 or service logs.
