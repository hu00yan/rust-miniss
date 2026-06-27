#!/bin/bash
# Run all benchmarks on Linux (io_uring required).
# Usage: ./bench_uring.sh

set -e

cd "$(dirname "$0")"
PROJECT=$(pwd)

echo "=== Building ==="
cargo build --release --features io_uring 2>&1

echo ""
echo "=== NOP Microbenchmark ==="
cargo run --release --features io_uring --example bench_nop 2>&1

echo ""
echo "=== HTTP Echo Comparison ==="
./bench/scripts/bench_http.sh 9091 15 100
