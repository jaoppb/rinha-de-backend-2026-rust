#!/usr/bin/env bash
set -euo pipefail

RESULTS_DIR="benchmark_results"
mkdir -p "$RESULTS_DIR"

echo "=== Phase 1: Baseline (Non-Verbose) ==="
make down
make build-release
make monitor | tee "$RESULTS_DIR/baseline.log"
cp test_stats.log "$RESULTS_DIR/baseline_stats.log" 2>/dev/null || true

echo "=== Phase 2: Analysis (Verbose) ==="
make down
make build-release-verbose
make monitor | tee "$RESULTS_DIR/verbose.log"
cp test_stats.log "$RESULTS_DIR/verbose_stats.log" 2>/dev/null || true

echo "=== Summary ==="
echo "Baseline results saved to $RESULTS_DIR/baseline.log"
echo "Verbose results saved to $RESULTS_DIR/verbose.log"
echo "Timing statistics saved to $RESULTS_DIR/verbose_stats.log"
