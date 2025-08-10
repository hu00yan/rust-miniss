# rust-miniss

[![CI](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml)
[![Docs](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml)
[![Benchmarks](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml)
[![Nightly](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml)
[![Sanitizers](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml)
[![Diff Coverage](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml)
[![Typos](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml)
[![Container Image](https://img.shields.io/badge/ghcr.io-rust--miniss-blue?logo=docker)](https://github.com/hu00yan/rust-miniss/pkgs/container/rust-miniss)

A Rust implementation of [miniss](https://github.com/qqiangwu/miniss) - a toy version of the [Seastar](https://github.com/scylladb/seastar) framework.

## Overview

rust-miniss is a minimal async runtime that demonstrates high-performance server design principles:

- **Shared-nothing architecture**: Each CPU core runs independently
- **Lock-free communication**: Cross-CPU messaging via SPSC queues
- **Zero-copy I/O**: Using io-uring on Linux
- **Custom futures**: Understanding async internals

## HTTP Echo Example & Benchmarks

- Example: `examples/http_echo.rs` (HTTP/1.1 minimal echo server)
- Run:
  - Build release: `cargo build --release --examples`
  - Start server: `./target/release/examples/http_echo --addr 127.0.0.1:8080`
  - Benchmark with wrk: `wrk -t4 -c256 -d30s http://127.0.0.1:8080/`
- Results (macOS, localhost):
  - Requests/sec: 21978.09
  - p99 latency: N/A (wrk output format for this run didn’t expose p99)
- More details: see docs/benchmarks/http.md

## Goals

1. **Educational**: Learn how async runtimes work internally
2. **Performance**: Achieve near-optimal performance for specific use cases
3. **Simplicity**: Keep the codebase small and understandable (~2000 lines)

## Architecture

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   CPU 0     │     │   CPU 1     │     │   CPU 2     │
│  ┌───────┐  │     │  ┌───────┐  │     │  ┌───────┐  │
│  │Tasks  │  │────▶│  │Tasks  │  │────▶│  │Tasks  │  │
│  └───────┘  │◀────│  └───────┘  │◀────│  └───────┘  │
│  ┌───────┐  │     │  ┌───────┐  │     │  ┌───────┐  │
│  │IO Ring│  │     │  │IO Ring│  │     │  │IO Ring│  │
│  └───────┘  │     │  └───────┘  │     │  └───────┘  │
└─────────────┘     └─────────────┘     └─────────────┘
     SPSC Queue          SPSC Queue
```

## Building

```bash
# For Linux with io-uring support (recommended for performance)
cargo build --features io_uring

# For macOS/other platforms (fallback mode)
cargo build
```


## Examples

[![HTTP benchmark](https://img.shields.io/badge/http%20echo-benchmark-blue)](docs/benchmarks/http.md)

- HTTP Echo: examples/http_echo.rs (see benchmark above)

### Using as a lightweight runtime/network library

- Add to your project: This crate exposes a minimal runtime and timer utilities; see docs/API_REFERENCE.md for APIs.
- Single-threaded runtime: Use Runtime::new().block_on(fut) to drive async code.
- Timers: Use timer::sleep, timer::timeout, and timer::Interval for scheduling.
- Multi-core (feature-gated): With `multicore`, distribute tasks across CPU executors.
- Networking: Use the HTTP echo example as a template for simple TCP-based protocols.
- Platform note: For highest I/O performance, prefer Linux with `io-uring` (e.g., in an OrbStack-backed container on macOS).

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

### Timer Utilities

The runtime provides several timer utilities for async timing operations:

```rust
use rust_miniss::{timer, Runtime};
use std::time::Duration;

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        // Sleep for 1 second
        timer::sleep(Duration::from_secs(1)).await;
        println!("Slept for 1 second");
        
        // Apply a timeout to an operation
        let result = timer::timeout(Duration::from_secs(2), async {
            timer::sleep(Duration::from_millis(500)).await;
            "Operation completed"
        }).await;
        
        match result {
            Ok(value) => println!("Success: {}", value),
            Err(_) => println!("Operation timed out"),
        }
        
        // Create a periodic interval
        let mut interval = timer::Interval::new(Duration::from_millis(200));
        for i in 0..3 {
            interval.tick().await;
            println!("Tick {}", i + 1);
        }
    });
}
```

### Periodic Tasks

Spawn tasks that run at regular intervals:

```rust
use rust_miniss::{task, timer, Runtime};
use std::time::Duration;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        
        // Spawn a periodic task
        let handle = task::spawn_periodic(Duration::from_millis(100), move || {
            let counter = counter_clone.clone();
            async move {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                println!("Periodic task executed: {}", count + 1);
            }
        }).unwrap();
        
        // Let it run for a while
        timer::sleep(Duration::from_millis(550)).await;
        
        // Cancel the periodic task
        handle.cancel().unwrap();
        println!("Final count: {}", counter.load(Ordering::SeqCst));
    });
}
```

### Graceful Shutdown via Signals

Handle system signals for graceful application shutdown:

```rust
#[cfg(feature = "signal")]
use rust_miniss::{Runtime, timer, signal};
use std::time::Duration;

#[cfg(feature = "signal")]
fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        // Set up signal handling for graceful shutdown
        let shutdown_signal = signal::wait_for_signal(&["SIGTERM", "SIGINT"]);
        
        // Your main application logic
        let main_task = async {
            let mut counter = 0;
            loop {
                timer::sleep(Duration::from_millis(500)).await;
                counter += 1;
                println!("Working... iteration {}", counter);
                
                // Simulate some work being done
                if counter >= 20 {
                    println!("Work completed naturally");
                    break;
                }
            }
        };
        
        // Wait for either the main task to complete or a shutdown signal
        tokio::select! {
            _ = main_task => {
                println!("Main task completed successfully");
            }
            signal = shutdown_signal => {
                println!("Received signal: {:?}, shutting down gracefully...", signal);
                
                // Perform cleanup operations
                println!("Cleaning up resources...");
                timer::sleep(Duration::from_millis(100)).await;
                
                // Close connections, flush data, etc.
                println!("Cleanup completed, exiting");
            }
        }
    });
}

#[cfg(not(feature = "signal"))]
fn main() {
    println!("Signal handling example requires the 'signal' feature");
    println!("Run with: cargo run --features signal --example graceful_shutdown");
}
```

## Docker/OrbStack Development

This project includes Docker containers for development. To clean up Docker/OrbStack resources and free up disk space, use the provided cleanup script:

```bash
./cleanup.sh
```

The script will:
- Show current disk usage
- Remove stopped containers
- Remove dangling images
- Optionally remove unused volumes (with confirmation)
- Remove unused networks
- Display final disk usage

### Manual Cleanup Commands

For manual cleanup, you can run these commands individually:

```bash
# List current objects
docker ps -a
docker images -a
docker volume ls
docker network ls

# Remove specific containers and images
docker rm <container_id>
docker rmi <image_id>

# Prune unused resources
docker container prune -f    # Remove stopped containers
docker image prune -f        # Remove dangling images
docker volume prune -f       # Remove unused volumes
docker network prune -f      # Remove unused networks

# Check disk usage
docker system df
```

For more information about Docker best practices, see the [OrbStack documentation](https://docs.orbstack.dev/).

## Status

This is a learning project and work in progress. See [DEVELOPMENT_PLAN.md](DEVELOPMENT_PLAN.md) for the roadmap.

- Project purpose and usage notes: see [docs/USAGE_AND_POSITIONING.md](docs/USAGE_AND_POSITIONING.md)

## License

MIT OR Apache-2.0
