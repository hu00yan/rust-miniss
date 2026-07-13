# rust-miniss

A thread-per-core async runtime for Linux and macOS. Linux uses **io_uring** for the performance path; macOS uses kqueue for the correctness and development path. Inspired by [Seastar](https://github.com/scylladb/seastar) and [Monoio](https://github.com/bytedance/monoio).

Performance is tracked as reproducible Linux snapshots, not as a permanent
throughput claim. See [the current baseline](docs/review/performance-baseline-2026-07-11.md).

## Overview

- **thread-per-core**: Each CPU core runs independently with its own io_uring instance
- **lock-free**: Zero shared mutable state between cores; SPSC queues for cross-core messages
- **I/O backends**: Linux uses io_uring (with epoll fallback); macOS uses kqueue.
- **HTTP/1.1 + HTTP/2 + QUIC/HTTP-3**: Bounded integration tests run on both supported platforms.

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
# Linux (io_uring is selected automatically when available)
cargo build --all-features

# macOS (kqueue)
cargo build
```

## I/O Backend Selection

| Platform | Backend | Notes |
|----------|---------|-------|
| Linux with io_uring support | io_uring | Performance path |
| macOS | kqueue | Correctness/testing path |

## HTTP Echo Benchmark (Linux)

```bash
cargo build --release --example http_echo
./target/release/examples/http_echo --addr 0.0.0.0:8080
wrk -t4 -c100 -d30s http://127.0.0.1:8080
```

Do not infer performance from this README. Run the documented benchmark matrix
and retain host, kernel, workload, throughput, and p99 metadata.

## Available Examples

- `examples/hello_world.rs` — Single-threaded runtime primer
- `examples/http_echo.rs` — HTTP/1.1 echo server (the primary benchmark)
- `examples/udp_buffered.rs` — UDP socket with buffered I/O
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
