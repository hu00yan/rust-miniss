# HTTP Echo Benchmark

This document captures benchmark results for the minimal HTTP/1.1 echo server across different async runtimes.

## Test Setup

### Hardware
- **CPU**: Intel/AMD x86_64 multi-core
- **Memory**: 8GB+
- **OS**: Linux (Ubuntu 24.04 LTS) or macOS
- **Rust**: Latest stable

### Benchmark Tool
- **wrk**: `wrk -t2 -c50 -d10s http://127.0.0.1:8080/`
- **Connections**: 50 concurrent
- **Threads**: 2
- **Duration**: 10 seconds

## How to Run Benchmarks

### Running rust-miniss

```bash
# Build with io-uring support (Linux)
cargo build --release --example http_echo --features io_uring

# Run the server
./target/release/examples/http_echo --addr 127.0.0.1:8080

# Benchmark
wrk -t2 -c50 -d10s http://127.0.0.1:8080/
```

### Running on macOS

```bash
# Build (uses kqueue via mio)
cargo build --release --example http_echo

# Run the server
./target/release/examples/http_echo --addr 127.0.0.1:8080

# Benchmark
wrk -t2 -c50 -d10s http://127.0.0.1:8080/
```

## Performance Characteristics

### rust-miniss Characteristics
- **Strengths**:
  - Thread-per-core architecture eliminates contention
  - Lock-free cross-CPU communication
  - Panic isolation between tasks
  - Low memory footprint

- **I/O Backend Performance**:
  - **io-uring (Linux 5.10+)**: Best performance with completion-based I/O
  - **epoll/kqueue**: Good performance with readiness-based I/O

### Optimization Opportunities
1. **Scheduler**: Implement more sophisticated task distribution
2. **I/O**: Optimize AsyncRead/AsyncWrite implementations
3. **Multi-core**: Enable thread-per-core mode for HTTP server
4. **Zero-copy**: Improve buffer management

## Environment Details

For reproducible results, record:
- **CPU**: `lscpu | grep "Model name"`
- **Memory**: `free -h`
- **Kernel**: `uname -r`
- **Rust**: `rustc -V`
- **I/O Backend**: io_uring vs epoll vs kqueue

## Benchmark Commands

```bash
# Run Criterion micro-benches
cargo bench --bench perf_benchmark
cargo bench --bench memory_perf

# Build the glommio baseline (if available)
cargo build --manifest-path bench/runtime_comparison/Cargo.toml --release --bin glommio_echo

# Compare against rust-miniss
./target/release/examples/http_echo --addr 0.0.0.0:8080 &
wrk -t4 -c100 -d30s http://127.0.0.1:8080
```

