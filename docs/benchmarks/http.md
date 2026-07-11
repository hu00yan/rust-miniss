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

## Metric Priority

### P0
- `requests_per_sec`
- `p99_latency`
- `error_rate`

### P1
- `p50_latency`
- `p90_latency`
- `cpu_utilization`
- `memory_usage`

### P2
- `syscalls_per_sec`
- `context_switches`
- `cache_miss_rate`

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

For baseline runs on Linux, prefer:

```bash
HTTP_COLLECT_DIAGNOSTICS=1 bench/scripts/bench_http_matrix.sh 15000
```

For a faster pass, use `HTTP_MATRIX_LIGHT=1` to keep the baseline run and trim the rest to miniss-only scenarios.

The default matrix now includes `ka800` and `close800` as the high-concurrency stress points. Treat those
as the main follow-up optimization indicators when comparing miniss, monoio, and compio.

This writes:
- `summary.csv` for requests/sec and latency percentiles
- `diagnostics.csv` for CPU, memory, syscall, context-switch, and cache metrics

For shard-level runtime diagnostics, set `MINISS_METRICS_INTERVAL_MS=1000` when running `examples/http_echo`.
It prints per-core `flushes`, `parks`, `park_timeouts`, `park_completions`, `busy_rounds`,
`message_batches`, `max_ready_queue_len`, `max_message_inbox_len`, and completion-wake split counts.

For a second protocol baseline, use:

```bash
bench/scripts/bench_tcp_pingpong.sh 19080
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

### Baseline Rule
- Do not compare a single run against another single run.
- Compare the same scenario set across at least one short-connection case and one keep-alive case.
- Treat a win as temporary until `summary.csv` and `diagnostics.csv` agree across repeat runs.
- Treat `http_echo --cores 1` as a diagnostic baseline only. It uses the same implementation as the multicore target; use it to isolate hot-path issues, not as the primary performance target.

## Environment Details

For reproducible results, record:
- **CPU**: `lscpu | grep "Model name"`
- **Memory**: `free -h`
- **Kernel**: `uname -r`
- **Rust**: `rustc -V`
- **I/O Backend**: io_uring vs epoll vs kqueue

## Benchmark Commands

```bash
# Run Criterion micro-benches first; treat these as the primary performance signal
cargo bench --bench perf_benchmark
cargo bench --bench memory_perf

# Use the HTTP matrix to validate end-to-end impact after a micro-bench change
HTTP_COLLECT_DIAGNOSTICS=1 bench/scripts/bench_http_matrix.sh 15000

# Build the glommio baseline (if available)
cargo build --manifest-path bench/runtime_comparison/Cargo.toml --release --bin glommio_echo

# Compare against rust-miniss
./target/release/examples/http_echo --addr 0.0.0.0:8080 &
wrk -t4 -c100 -d30s http://127.0.0.1:8080
```

## Interpretation Rule

- If a micro-benchmark regresses, treat that as a real bug until proven otherwise.
- If only an end-to-end benchmark changes, check whether the difference is load-shape specific before generalizing.
- Use `ka8`, `ka200`, and `ka800` to find inflection points, not to define success by themselves.
