#!/bin/bash
# HTTP Benchmark Script for Runtime Comparison

set -euo pipefail

# Configuration
PORT=${1:-8080}
DURATION=${2:-30}
CONNECTIONS=${3:-100}
ADDR="http://127.0.0.1:$PORT"

echo "🚀 HTTP Benchmark for rust-miniss vs tokio/monoio/glommio"
echo "Port: $PORT, Duration: ${DURATION}s, Connections: $CONNECTIONS"
echo

# Function to run benchmark
run_benchmark() {
    local runtime=$1
    local binary=$2
    
    echo "📊 Benchmarking $runtime..."
    
    # Start server in background
    $binary --addr "0.0.0.0:$PORT" &
    local server_pid=$!
    
    # Wait for server to start
    sleep 2
    
    # Run benchmark with wrk
    echo "Running wrk benchmark..."
    wrk -t4 -c$CONNECTIONS -d${DURATION}s --timeout 10s $ADDR 2>/dev/null || {
        echo "wrk benchmark failed for $runtime"
        kill $server_pid 2>/dev/null || true
        return 1
    }
    
    # Kill server
    kill $server_pid 2>/dev/null || true
    wait $server_pid 2>/dev/null || true
    
    echo
}

# Check if wrk is installed
if ! command -v wrk &> /dev/null; then
    echo "❌ wrk is not installed. Please install it first."
    exit 1
fi

# Build all binaries
echo "🔨 Building binaries..."
cargo build --release --example http_echo
cargo build --release --bin tokio_echo || echo "tokio_echo not available"

# Run benchmarks
echo "📈 Starting benchmarks..."
echo

# rust-miniss
if [ -f target/release/examples/http_echo ]; then
    run_benchmark "rust-miniss" "target/release/examples/http_echo"
else
    echo "❌ rust-miniss http_echo not found"
fi

# tokio (if available)
if [ -f target/release/tokio_echo ]; then
    run_benchmark "tokio" "target/release/tokio_echo"
elif [ -f bench/runtime_comparison/tokio_echo.rs ]; then
    echo "📦 Building tokio comparison binary..."
    cd bench/runtime_comparison
    cargo build --release --bin tokio_echo 2>/dev/null || {
        echo "⚠️  Tokio comparison binary build failed"
    }
    cd ../..
    if [ -f bench/runtime_comparison/target/release/tokio_echo ]; then
        run_benchmark "tokio" "bench/runtime_comparison/target/release/tokio_echo"
    fi
else
    echo "⚠️  Tokio comparison not available"
fi

echo "✅ Benchmark completed!"