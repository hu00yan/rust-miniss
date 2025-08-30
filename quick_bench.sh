#!/bin/bash
# Quick performance comparison script - 3 minutes max

set -euo pipefail

echo "🚀 Quick Performance Test: rust-miniss vs tokio"
echo "Duration: 10 seconds each, Connections: 100"
echo

PORT=8081
DURATION=10
CONNECTIONS=100

# Function to run quick benchmark
quick_bench() {
    local runtime=$1
    local binary=$2
    
    echo "📊 Testing $runtime..."
    
    # Start server in background
    timeout 30s $binary --addr "0.0.0.0:$PORT" &
    local server_pid=$!
    
    # Wait for server to start
    sleep 1
    
    # Run benchmark with shorter duration
    echo "Running quick benchmark..."
    timeout 15s wrk -t2 -c$CONNECTIONS -d${DURATION}s --timeout 5s "http://127.0.0.1:$PORT" 2>/dev/null || {
        echo "❌ Benchmark failed for $runtime"
        kill $server_pid 2>/dev/null || true
        return 1
    }
    
    # Kill server
    kill $server_pid 2>/dev/null || true
    wait $server_pid 2>/dev/null || true
    
    echo
    sleep 1
}

# Check if wrk is installed
if ! command -v wrk &> /dev/null; then
    echo "❌ wrk is not installed. Installing..."
    sudo apt update && sudo apt install -y wrk
fi

# rust-miniss (if available)
if [ -f target/release/examples/http_echo ]; then
    quick_bench "rust-miniss" "target/release/examples/http_echo"
else
    echo "⚠️  rust-miniss http_echo not available, skipping..."
fi

# tokio
if [ -f bench/runtime_comparison/target/release/tokio_echo ]; then
    quick_bench "tokio" "bench/runtime_comparison/target/release/tokio_echo"
else
    echo "⚠️  tokio comparison binary not available"
fi

echo "✅ Quick benchmark completed!"