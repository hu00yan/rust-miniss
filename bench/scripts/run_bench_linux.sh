#!/bin/bash
# Run benchmark on Linux VM
set -euo pipefail

# Use regular user's rustup/rust/cargo
export RUSTUP_HOME=/home/huyan00/.rustup
export CARGO_HOME=/home/huyan00/.cargo
export PATH="/home/huyan00/.cargo/bin:$PATH"

cd /Users/huyan00/mycode/rust-miniss

# Kill any old servers
pkill -x http_echo 2>/dev/null || true
pkill -f monoio_echo 2>/dev/null || true
sleep 1

# ── 1. NOP microbenchmark ──
echo "=== NOP Microbenchmark ==="
cargo run --release --features io_uring --example bench_nop 2>&1
echo

# ── 2. rust-miniss HTTP echo (30s, 100 conn) ──
echo "=== rust-miniss HTTP echo (30s, 100 conn) ==="
RUST_LOG=error ./target/release/examples/http_echo --cores 1 --addr 0.0.0.0:8080 &
SERVER_PID=$!
sleep 2

# Verify
if ! kill -0 $SERVER_PID 2>/dev/null; then
    echo "Server failed to start!"
    exit 1
fi

# Warmup
wrk -t2 -c10 -d5s --timeout 5s http://127.0.0.1:8080/ > /dev/null 2>&1 || true

# Measured run
wrk -t4 -c100 -d30s --timeout 10s http://127.0.0.1:8080/ 2>&1

kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true
echo

# ── 3. perf stat syscall counting ──
echo "=== perf stat (syscalls) for 10s run ==="
RUST_LOG=error ./target/release/examples/http_echo --cores 1 --addr 0.0.0.0:8080 &
SERVER_PID=$!
sleep 2

# Start perf in background
perf stat -e syscalls:sys_enter_io_uring_enter -p $SERVER_PID -- sleep 10 &
PERF_PID=$!

# Run wrk for 10s
wrk -t4 -c100 -d10s --timeout 10s http://127.0.0.1:8080/ > /dev/null 2>&1 || true

wait $PERF_PID 2>/dev/null || true

kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true
echo

# ── 4. Build Monoio comparison ──
echo "=== Building Monoio echo server ==="
cd bench/runtime_comparison
cargo build --release --bin monoio_echo 2>&1
cd /Users/huyan00/mycode/rust-miniss

echo "=== Monoio HTTP echo (30s, 100 conn) ==="
./bench/runtime_comparison/target/release/monoio_echo --addr 0.0.0.0:8081 &
MONOIO_PID=$!
sleep 2

if ! kill -0 $MONOIO_PID 2>/dev/null; then
    echo "Monoio server failed to start!"
    exit 1
fi

wrk -t2 -c10 -d5s --timeout 5s http://127.0.0.1:8081/ > /dev/null 2>&1 || true
wrk -t4 -c100 -d30s --timeout 10s http://127.0.0.1:8081/ 2>&1

kill $MONOIO_PID 2>/dev/null || true
wait $MONOIO_PID 2>/dev/null || true
echo "=== Done ==="
