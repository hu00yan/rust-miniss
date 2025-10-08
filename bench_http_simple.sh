#!/bin/bash

echo "🚀 Simple HTTP Performance Comparison"
echo "====================================="

# Test rust-miniss
echo "Testing rust-miniss..."
timeout 15 cargo run --release --example http_echo -- --addr 127.0.0.1:8080 > /dev/null 2>&1 &
RUST_PID=$!
sleep 3

if kill -0 $RUST_PID 2>/dev/null; then
    echo "rust-miniss server started"
    wrk -t2 -c20 -d5s http://127.0.0.1:8080/ 2>/dev/null | grep "Requests/sec"
    kill $RUST_PID 2>/dev/null
    wait $RUST_PID 2>/dev/null
else
    echo "rust-miniss server failed to start"
fi

echo ""

# Test tokio
echo "Testing tokio..."
cd bench/runtime_comparison
./target/release/tokio_echo --addr 127.0.0.1:8080 > /dev/null 2>&1 &
TOKIO_PID=$!
sleep 3

if kill -0 $TOKIO_PID 2>/dev/null; then
    echo "tokio server started"
    wrk -t2 -c20 -d5s http://127.0.0.1:8080/ 2>/dev/null | grep "Requests/sec"
    kill $TOKIO_PID 2>/dev/null
    wait $TOKIO_PID 2>/dev/null
else
    echo "tokio server failed to start"
fi

cd ..
echo ""
echo "✅ Comparison complete"
