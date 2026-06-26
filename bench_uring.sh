#!/bin/bash
set -e

PROJECT=/mnt/mac/Users/huyan00/mycode/rust-miniss
cd "$PROJECT"

echo "=== Building ==="
cargo build --release --features io_uring 2>&1

echo ""
echo "=== Spawn Microbenchmark ==="
cargo run --release --example spawn_micro --features io_uring 2>&1

echo ""
echo "=== Starting echo server (no wrk test) ==="
cargo run --release --example http_echo_onecore --features io_uring -- --addr 127.0.0.1:9091 &
SERVER_PID=$!
sleep 2

# Test that server is running
if kill -0 $SERVER_PID 2>/dev/null; then
    echo "Server running (PID $SERVER_PID)"
else
    echo "SERVER FAILED TO START"
    exit 1
fi

echo "=== Test: wrk -t2 -c10 -d10s ==="
wrk -t2 -c10 -d10s --timeout 5s http://127.0.0.1:9091/ 2>&1 || echo "wrk failed"

echo ""
echo "=== Test: wrk -t4 -c100 -d15s ==="
wrk -t4 -c100 -d15s --timeout 5s http://127.0.0.1:9091/ 2>&1 || echo "wrk failed"

echo ""
echo "=== Test: wrk -t4 -c1024 -d15s ==="
wrk -t4 -c1024 -d15s --timeout 5s http://127.0.0.1:9091/ 2>&1 || echo "wrk failed"

kill $SERVER_PID 2>/dev/null
wait $SERVER_PID 2>/dev/null
echo ""
echo "=== Done ==="
