# HTTP Echo Benchmark

This document captures benchmark results for the minimal HTTP/1.1 echo server.

How to reproduce:
- Build release: `cargo build --release --examples`
- Run server: `./target/release/examples/http_echo --addr 127.0.0.1:8080`
- Benchmark: `wrk -t4 -c256 -d30s http://127.0.0.1:8080/`
- Compare with Tokio: `hyperfine './target/release/examples/http_echo --addr 127.0.0.1:8080' './target/release/http_echo_tokio --addr 127.0.0.1:8080'`

Results (captured on macOS, localhost):
- Requests/sec: 21978.09
- p99 latency: N/A (wrk output format did not expose p99 in this run)

Note:
- Authoritative/representative performance runs should be executed on Linux with `io-uring` enabled. Non-Linux results are informative for development only.

Hyperfine:
- Could not capture due to long-running server model; consider adding a `--once` flag to servers for hyperfine micro-benchmarks, or using hyperfine with a wrapper script that starts, probes, and terminates.

Environment (record this for each run):
- CPU: e.g., `sysctl -n machdep.cpu.brand_string` on macOS, or `/proc/cpuinfo | grep 'model name' | head -1` on Linux
- Memory: e.g., `vm_stat` (macOS) or `free -h` (Linux)
- OS: `sw_vers` (macOS) or `/etc/os-release` (Linux)
- Rust: `rustc -V`
- Tools: wrk, hyperfine

