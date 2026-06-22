# rust-miniss

[![CI](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml)
[![Docs](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml)
[![Benchmarks](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml)
[![Nightly](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml)
[![Sanitizers](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml)
[![Diff Coverage](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml)
[![Typos](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml)
[![Container Image](https://img.shields.io/badge/ghcr.io-rust--miniss-blue?logo=docker)](https://github.com/hu00yan/rust-miniss/pkgs/container/rust-miniss)

A minimal thread-per-core async runtime for Rust, originally inspired by [Seastar](https://github.com/scylladb/seastar) but evolved into a Rust-specific implementation.

## Overview

rust-miniss is a minimal async runtime that demonstrates high-performance server design principles:

- **Thread-per-core architecture**: Each CPU core runs independently with its own event loop
- **Lock-free communication**: Cross-CPU messaging via SPSC queues
- **Multi-backend I/O**: io-uring (Linux 5.10+), epoll (Linux/Unix), kqueue (macOS)
- **futures::io compatibility**: Implements `AsyncRead`/`AsyncWrite` traits for ecosystem integration
- **Panic isolation**: Task panics don't affect other tasks
- **HTTP/1.1 + HTTP/2**: Integration with hyper ecosystem
- **QUIC + HTTP/3**: Integration with quinn and h3 ecosystems

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
        // Sleep for 1 second
        timer::sleep(Duration::from_secs(1)).await;
        
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

## HTTP and QUIC Support

### HTTP/1.1 + HTTP/2

rust-miniss integrates with the [hyper](https://hyper.rs) ecosystem for HTTP support:

```rust
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use rust_miniss::net::AsyncTcpListener;
use rust_miniss::hyper_compat::{HyperExecutor, MinissIo};

async fn handle(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    Ok(Response::new(Full::new(Bytes::from("Hello from rust-miniss!"))))
}

fn main() {
    let runtime = rust_miniss::MultiCoreRuntime::new(Some(1)).unwrap();
    
    runtime.block_on(async {
        let listener = AsyncTcpListener::bind("0.0.0.0:8080").unwrap();
        
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            HyperExecutor::spawn(async move {
                let io = MinissIo::new(stream);
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service_fn(handle))
                    .await
                    .unwrap();
            });
        }
    });
}
```

### QUIC + HTTP/3

For QUIC and HTTP/3 support, rust-miniss integrates with [quinn](https://github.com/quinn-rs/quinn) and [h3](https://github.com/hyperium/h3):

```rust
use rust_miniss::quinn_compat::MinissQuinnRuntime;
use std::sync::Arc;

fn main() {
    let runtime = rust_miniss::MultiCoreRuntime::new(Some(1)).unwrap();
    
    runtime.block_on(async {
        // Setup QUIC endpoint
        let endpoint = setup_quic_endpoint().unwrap();
        
        while let Some(conn) = endpoint.accept().await {
            tokio::spawn(async move {
                let conn = conn.await.unwrap();
                // Handle QUIC connection
            });
        }
    });
}
```

For detailed examples, see:
- `examples/http1_server.rs` - HTTP/1.1 server
- `examples/http2_server.rs` - HTTP/2 server
- `examples/http3_server.rs` - HTTP/3 server with QUIC

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
│             │             │             │                     │
│ ┌─────────┐ │ ┌─────────┐ │ ┌─────────┐ │ ┌─────────────────┐ │
│ │Timer    │ │ │Timer    │ │ │Timer    │ │ │Cross-CPU        │ │
│ │Wheel    │ │ │Wheel    │ │ │Wheel    │ │ │Message Bus      │ │
│ └─────────┘ │ └─────────┘ │ └─────────┘ │ └─────────────────┘ │
│             │             │             │                     │
│ ┌─────────┐ │ ┌─────────┐ │ ┌─────────┐ │                     │
│ │I/O      │ │ │I/O      │ │ │I/O      │ │                     │
│ │Backend  │ │ │Backend  │ │ │Backend  │ │                     │
│ │(uring/  │ │ │(uring/  │ │ │(uring/  │ │                     │
│ │epoll/   │ │ │epoll/   │ │ │epoll/   │ │                     │
│ │kqueue)  │ │ │kqueue)  │ │ │kqueue)  │ │                     │
│ └─────────┘ │ └─────────┘ │ └─────────┘ │                     │
└─────────────┴─────────────┴─────────────┴─────────────────────┘
     SPSC Queue          SPSC Queue
```

## Building

```bash
# For Linux with io-uring support (recommended for performance)
cargo build --features io_uring

# For macOS (uses kqueue via mio)
cargo build

# For Linux with epoll (default)
cargo build --features epoll
```

## I/O Backend Selection

The runtime automatically selects the best I/O backend based on platform and kernel version:

| Platform | Backend | Feature Flag | Notes |
|----------|---------|--------------|-------|
| Linux 5.10+ | io-uring | `io_uring` | Best performance |
| Linux (older) | epoll | `epoll` | Default on Linux |
| macOS | kqueue | `kqueue` | Via mio |
| Other Unix | epoll | `epoll` | Fallback |

## Examples

- `examples/hello_world.rs` – Single-threaded runtime primer
- `examples/http_echo.rs` – HTTP/1.1 echo server
- `examples/udp_buffered.rs` – UDP socket with buffered I/O
- `examples/metrics_server.rs` – Prometheus metrics exporter

### HTTP Echo Server

```bash
# Build and run
cargo build --release --example http_echo
./target/release/examples/http_echo --addr 0.0.0.0:8080

# Test with curl
curl http://127.0.0.1:8080

# Benchmark with wrk
wrk -t4 -c100 -d30s http://127.0.0.1:8080
```

## Goals

1. **Educational**: Learn how async runtimes work internally
2. **Performance**: Achieve near-optimal performance for specific use cases
3. **Simplicity**: Keep the codebase small and understandable

### Development Workflow
```bash
# 1. Write code
# 2. Auto format
make fmt-fix

# 3. Quick validation
make quick

# 4. Run tests
make test

# 5. Pre-commit check
make pre-commit
```

### CI Troubleshooting
| CI Job | Local Command |
|--------|---------------|
| Quick Checks | `make quick` |
| Test Suite | `make check` |
| ASan+LSan | `make asan` |
| TSan | `make tsan` |
| Miri | `make miri` |

## HTTP Echo Example & Benchmarks

The primary sample program is `examples/http_echo.rs`, a minimal HTTP/1.1 echo server that now runs entirely within the runtime thread so drops never access I/O state from foreign threads.

### Running the example

```bash
# Build the runtime examples
cargo build --release --examples

# Start the HTTP echo server
./target/release/examples/http_echo --addr 0.0.0.0:8080

# In another terminal run a benchmark (OrbStack Ubuntu guest shown)
wrk -t4 -c100 -d30s http://127.0.0.1:8080
```

### wrk (t4 / c100 / 30s) on Linux guest

| Runtime | Requests/sec | Transfer/sec | Notes |
|---------|--------------|--------------|-------|
| **rust-miniss** | 100,367 | 7.56 MB/s | `wrk` reports read errors because the server closes each response eagerly; throughput is stable |
| **glommio**     | 100,665 | 7.58 MB/s | Baseline built from `bench/runtime_comparison/glommio_echo.rs` |

The close parity shows that the io_uring backend and buffer pool are functioning correctly under sustained load.

### Benchmark suite layout

- `bench/internal/` – micro benchmarks driven by `cargo bench`
- `bench/criterion/` – Criterion harnesses for runtime/timer profiling
- `bench/runtime_comparison/` – external echo servers (currently glommio) used for wrk comparisons

Helpful commands:

```bash
# Run Criterion micro-benches
cargo bench --bench perf_benchmark
cargo bench --bench memory_perf

# Build the glommio baseline
cargo build --manifest-path bench/runtime_comparison/Cargo.toml --release --bin glommio_echo

# Compare against rust-miniss
./target/release/examples/http_echo --addr 0.0.0.0:8080 &
wrk -t4 -c100 -d30s http://127.0.0.1:8080
```

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

- HTTP Echo: `examples/http_echo.rs` (see benchmark above)

### Available examples

- `examples/hello_world.rs` – single-threaded runtime primer
- `examples/http_echo.rs` – end-to-end HTTP/1.1 echo server
- `examples/udp_buffered.rs` – UDP socket with pooled buffers
- `examples/metrics_server.rs` – Prometheus exporter wired into the runtime

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
use rust_miniss::{timer, Runtime};
use std::time::Duration;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        
        // Spawn a periodic task using a timer loop
        let handle = runtime.spawn(async move {
            for _ in 0..5 {
                timer::sleep(Duration::from_millis(100)).await;
                let count = counter_clone.fetch_add(1, Ordering::SeqCst);
                println!("Periodic task executed: {}", count + 1);
            }
        });
        
        // Let it run for a while
        timer::sleep(Duration::from_millis(550)).await;
        
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

This project includes Docker containers for development. If disk usage grows, rely on Docker's built-in pruning commands:

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

For more guidance on Docker maintenance with OrbStack, see the [OrbStack documentation](https://docs.orbstack.dev/).

## Safety and Design Principles

### Unsafe Code Usage

This project uses unsafe code for performance-critical I/O operations. Key principles:

1. **Minimal unsafe scope**: Unsafe blocks are kept as small as possible
2. **Detailed safety comments**: Every unsafe block has documentation explaining why it's safe
3. **RAII compliance**: All resources are properly managed through Rust's ownership system
4. **Test coverage**: Unsafe code has comprehensive test coverage

### Memory Safety Testing

- **AddressSanitizer (ASan)**: Detects memory errors
- **LeakSanitizer (LSan)**: Detects memory leaks
- **ThreadSanitizer (TSan)**: Detects data races
- **Miri**: Detects undefined behavior

### Known Limitations

- Miri doesn't support io-uring syscalls (expected behavior)
- Some proc-macro crates may not be fully compatible with sanitizers
- Sanitizer tests are primarily for CI validation

## Known Issues

### Platform Support

- **Linux**: Full support with io-uring or epoll backends
- **macOS**: Development support with kqueue backend
- **Windows**: Not supported (contributions welcome)

### Feature Completeness

- Some operations (UdpRecvVectored, UdpSendVectored, ReadRent, WriteRent) are not yet implemented in all backends
- The `AsyncUnixStream` and `AsyncUnixListener` types have stub implementations
- Signal handling requires the `signal` feature flag

## Status

This is a learning project and work in progress. See [docs/USAGE_AND_POSITIONING.md](docs/USAGE_AND_POSITIONING.md) for more details on project purpose and usage.

## License

MIT OR Apache-2.0
