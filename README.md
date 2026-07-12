# rust-miniss

[![CI](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml)
[![Nightly](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml)
[![Sanitizers](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml)

A thread-per-core async runtime for Linux, built on **io_uring**. Inspired by [Seastar](https://github.com/scylladb/seastar) and [Monoio](https://github.com/bytedance/monoio).

**Target: 650K+ req/s HTTP echo on 1 core (Monoio-class).**
(Current: ~239K, debugging in progress.)

## Overview

- **thread-per-core**: Each CPU core runs independently with its own io_uring instance
- **lock-free**: Zero shared mutable state between cores; SPSC queues for cross-core messages
- **io_uring primary, epoll/kqueue dev only**: Linux 6.12+ required for io_uring; macOS/other Unix get epoll/kqueue but are never optimized
- **HTTP/1.1 + HTTP/2 + QUIC/HTTP-3**: Integration under reconstruction — see [DESIGN.md](docs/DESIGN.md)

## Quick Start

### Basic Usage

```rust
use rust_miniss::Runtime;

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        println!("Hello from rust-miniss!");
    });
}
```

### Multi-core Usage

```rust
use rust_miniss::multicore::MultiCoreRuntime;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = MultiCoreRuntime::new(Some(4))?; // 4 CPU cores

    runtime.spawn(async {
        println!("Task running on a CPU core");
    })?;

    runtime.shutdown()?;
    runtime.join()?;
    Ok(())
}
```

### Timer Utilities

```rust
use rust_miniss::{timer, Runtime};
use std::time::Duration;

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        timer::sleep(Duration::from_secs(1)).await;

        let result = timer::timeout(Duration::from_secs(2), async {
            timer::sleep(Duration::from_millis(500)).await;
            "completed"
        }).await;

        let mut interval = timer::Interval::new(Duration::from_millis(200));
        for i in 0..3 {
            interval.tick().await;
            println!("Tick {}", i + 1);
        }
    });
}
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Multi-Core Runtime                           │
├─────────────┬─────────────┬─────────────┬─────────────────────┤
│   CPU 0     │   CPU 1     │   CPU 2     │   Signal Handler    │
│             │             │             │                     │
│ ┌─────────┐ │ ┌─────────┐ │ ┌─────────┐ │ ┌─────────────────┐ │
│ │Task     │ │ │Task     │ │ │Task     │ │ │OS Signal        │ │
│ │Queue    │ │ │Queue    │ │ │Queue    │ │ │Listener         │ │
│ └─────────┘ │ └─────────┘ │ └─────────┘ │ └─────────────────┘ │
│ ┌─────────┐ │ ┌─────────┐ │ ┌─────────┐ │                     │
│ │Timer    │ │ │Timer    │ │ │Timer    │ │ ┌─────────────────┐ │
│ │Wheel    │ │ │Wheel    │ │ │Wheel    │ │ │Cross-CPU        │ │
│ └─────────┘ │ └─────────┘ │ └─────────┘ │ │Message Bus      │ │
│ ┌─────────┐ │ ┌─────────┐ │ ┌─────────┐ │ └─────────────────┘ │
│ │io_uring │ │ │io_uring │ │ │io_uring │ │                     │
│ │instance │ │ │instance │ │ │instance │ │                     │
│ └─────────┘ │ └─────────┘ │ └─────────┘ │                     │
└─────────────┴─────────────┴─────────────┴─────────────────────┘
     SPSC Queue          SPSC Queue
```

### Event Loop (per core)

```
1. process_messages()    — cross-core task spawn / shutdown
2. poll_io()             — drain CQEs, inline poll when possible
3. execute_tasks()       — poll all ready tasks
4. flush()               — one io_uring_enter for all batched SQEs
5. process_timers()      — expire timer wheel
6. park_if_idle()        — submit_and_wait(1), no busy-wait
```

See [DESIGN.md](docs/DESIGN.md) for full design rationale.

## Building

```bash
# Linux (io_uring) — requires kernel 6.12+
cargo build --features io_uring

# macOS (kqueue, dev only)
cargo build
```

## I/O Backend Selection

| Platform | Backend | Notes |
|----------|---------|-------|
| Linux 6.12+ | io_uring | Performance target |
| macOS | kqueue | Dev only, never optimized |
| Other Unix | epoll | Dev only, never optimized |

## HTTP Echo Benchmark (Linux, io_uring, 1 core)

```bash
cargo build --release --example http_echo
./target/release/examples/http_echo --addr 0.0.0.0:8080
wrk -t4 -c100 -d30s http://127.0.0.1:8080
```

| Runtime | Requests/sec | Notes |
|---------|--------------|-------|
| rust-miniss | ~100K | Current baseline; targets 650K+ |
| glommio    | ~100K | Same level (benchmarked for reference) |

## Available Examples

- `examples/hello_world.rs` — Single-threaded runtime primer
- `examples/http_echo.rs` — HTTP/1.1 echo server (the primary benchmark)
- `examples/udp_buffered.rs` — UDP socket with buffered I/O
- `examples/metrics_server.rs` — Prometheus metrics exporter
- `examples/spawn_micro.rs` — Task spawn latency microbenchmark

## Goals

1. **Educational**: Learn how async runtimes work internally
2. **Performance**: Achieve near-optimal throughput for specific use cases
3. **Simplicity**: Keep the codebase small and understandable

## Safety and Design Principles

- Minimal unsafe code, well-documented with safety rationales
- All unsafe blocks are audited and bounded
- Lock-free data structures from `crossbeam` crate
- Proper pinning for futures and async state

### Memory Safety Testing

- **AddressSanitizer (ASan)**: Detects memory errors
- **LeakSanitizer (LSan)**: Detects memory leaks
- **ThreadSanitizer (TSan)**: Detects data races
- **Miri**: Detects undefined behavior (io_uring syscalls excluded)

## Status

Active development. Core io_uring backend, task system, and timer wheel are functional.
Hyper/Quinn/TLS integrations are **under reconstruction** — the previous implementations
were broken (busy-spin on WouldBlock, tokio::spawn in thread-per-core context). See
[DESIGN.md](docs/DESIGN.md) for the correct approach.

## License

MIT OR Apache-2.0
