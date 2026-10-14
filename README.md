# rust-miniss

A thread-per-core async runtime for Linux and macOS. Linux uses **io_uring** for the performance path; macOS uses kqueue for the correctness and development path. Inspired by [Seastar](https://github.com/scylladb/seastar) and [Monoio](https://github.com/bytedance/monoio).

Performance is tracked as reproducible Linux snapshots, not as a permanent
throughput claim. See [the current baseline](docs/review/performance-baseline-2026-07-11.md).

## Overview

- **thread-per-core**: Each CPU core runs independently with its own io_uring instance
- **lock-free**: Zero shared mutable state between cores; SPSC queues for cross-core messages
- **I/O backends**: Linux requires kernel 6.12+ and uses io_uring; macOS uses kqueue.
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

### Complete correctness suite

Run `just test-full` for the complete non-benchmark correctness suite. It runs
the feature compilation matrix, one all-feature regular regression pass,
production-linked differential tests, Loom, and bounded stress tests; Linux adds
the io_uring safety gate and a host with nightly Miri
adds the Miri gate. Any unavailable environment-specific gate is reported as
`SKIP`, never silently treated as passed.

### Memory Safety Testing

- **AddressSanitizer (ASan)**: Detects memory errors
- **LeakSanitizer (LSan)**: Detects memory leaks
- **ThreadSanitizer (TSan)**: Detects data races
- **Miri**: Detects undefined behavior (io_uring syscalls excluded)

## Status

Active development. The core io_uring backend, task system, timer wheel, HTTP/1.1,
Hyper HTTP/2, and Quinn/H3 integrations are functional for the tested loopback
scenarios. HTTP/2 and QUIC/H3 have bounded integration checks on macOS/kqueue and
Orb Linux/io_uring; TLS currently has an implemented client stream and compile/
ownership coverage, plus bounded local TLS 1.3 handshake, certificate rejection,
hostname mismatch, read/write, and close tests.
This is tested integration, not complete protocol-compatibility coverage:
TLS default-root/platform matrices, sustained multicore QUIC/UDP ownership stress,
and long-running kernel/performance matrices remain open. A macOS/Linux CI gate
now runs the supported feature tests and Linux safety checks. See
[the protocol record](docs/review/protocol-integration-2026-07-13.md) and
[DESIGN.md](docs/DESIGN.md).

### Position relative to monoio and compio

The main differences are API and ecosystem maturity, not the core scheduler
model:

- Like monoio and compio, `rust-miniss` uses a thread-per-core model for its
  data plane; it is not a Tokio-style work-stealing runtime with transparent
  task migration. Cross-thread dispatch or blocking offload, where provided,
  is an explicit operation rather than migration of an existing task.
- Its public API is smaller and its production ecosystem is much less mature.
- TLS is currently a client stream; HTTP/2 and HTTP/3 are integrations through
  Hyper and Quinn/H3 rather than a complete protocol framework.
- Linux and macOS are the supported platforms; the Linux path requires a recent
  io_uring-capable kernel.

These are the project's current boundaries. io_uring mechanisms such as linked
SQEs, provided buffers, multishot receive, registered buffers, and SQPOLL are
implementation techniques, not required public API features. The runtime should
choose them internally when they preserve the same API semantics; exposing raw
kernel flags would be an abstraction leak.

## License

MIT OR Apache-2.0
