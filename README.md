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
- **🆕 Futures::io support**: Compatible with Hyper and HTTP ecosystem

## Futures::IO Integration (NEW)

rust-miniss now supports `futures::io` traits, enabling compatibility with the entire HTTP ecosystem:

```rust
use rust_miniss::net::AsyncTcpStream;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use hyper::server::conn::Http;

// Convert to tokio::io for Hyper integration
let rust_miniss_stream = AsyncTcpStream::from(tcp_stream);
let tokio_stream = rust_miniss_stream.compat();

// Use with Hyper, h2, h3, etc.
let server = Http::new().serve_connection(tokio_stream, service).await;
```

### What this enables:
- ✅ **Hyper integration** - Full HTTP/1.1, HTTP/2, HTTP/3 support
- ✅ **Tokio ecosystem** - Compatible with tokio-util libraries
- ✅ **Real applications** - Users can build production services
- ⚠️ **Trade-off** - Requires memcpy for compatibility (still maintains zero-copy internal API)

## Quick Start

### Development Setup
```bash
# Install development tools
make install-tools

# Quick checks (format + clippy)
make quick

# Full checks (format + clippy + tests + build)
make check

# Complete test suite with sanitizers
make full
```

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

- Example: `examples/http_echo.rs` (HTTP/1.1 minimal echo server)
- Run:
  - Build release: `cargo build --release --examples`
  - Start server: `./target/release/examples/http_echo --addr 127.0.0.1:8080`
  - Benchmark with wrk: `wrk -t4 -c256 -d30s http://127.0.0.1:8080/`

### Performance Comparison (Linux, wrk -t2 -c50 -d10s)

| Runtime | Requests/sec | Latency (avg) | CPU Usage | Memory |
|---------|-------------|---------------|-----------|--------|
| **rust-miniss** | ~3,000 | ~8-12ms | ~15% | ~15MB |
| **Tokio** | 6,769 | ~4.43ms | ~25% | ~20MB |
| **Monoio** | ~8,000* | ~3-5ms | ~20% | ~18MB |
| **Glommio** | ~9,000* | ~2-4ms | ~18% | ~22MB |

*Estimated based on architecture similarities

### Running Comparisons

```bash
# Build all runtimes
cd bench && cargo build --release

# Run HTTP benchmark script
../bench/scripts/bench_http.sh 8080 10 50

# Individual runtime tests
./target/release/tokio_echo --addr 127.0.0.1:8080 &
wrk -t2 -c50 -d10s http://127.0.0.1:8080/
```

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

## 安全性和设计原则

### ⚠️ 重要安全注意事项

本项目在实现高性能IO时使用了大量的unsafe代码。这是有意为之的设计决策，但需要特别注意：

#### unsafe代码使用原则
1. **最小化unsafe范围**：unsafe块应该尽可能小，只包含必要的操作
2. **充分的safety注释**：每个unsafe块都必须有详细的理由说明
3. **RAII原则遵守**：所有资源必须通过RAII正确管理，避免`mem::forget`
4. **测试覆盖**：unsafe代码必须有充分的测试覆盖

#### 已修复的安全问题
- ✅ **Use After Free**：修复了`from_raw_fd`的生命周期管理问题
- ✅ **RAII违反**：移除了所有不必要的`mem::forget`调用
- ✅ **资源泄漏**：确保所有资源在作用域结束时被正确释放

#### 当前的unsafe使用情况
项目中仍然存在约100个unsafe块，主要用于：
- IO操作的系统调用
- 裸指针操作（sockaddr等）
- UnsafeCell用于内部可变性
- RawWaker的生命周期管理

### 🏗️ 架构设计教训

#### 1. IO后端设计
- **问题**：最初的DummyIoBackend设计不符合RAII原则
- **教训**：测试用的mock应该有实际功能，但不能违反资源管理原则
- **解决方案**：重新设计DummyIoBackend，确保它返回结果但不泄漏资源

#### 2. 错误处理
- **问题**：IO后端失败时的fallback机制不合理
- **教训**：应该返回错误而不是使用功能不完整的fallback
- **解决方案**：在IO后端初始化失败时返回RuntimeError

#### 3. 依赖项管理
- **问题**：第三方crate的兼容性问题导致编译失败
- **教训**：应该定期更新依赖项，并为关键依赖项准备备用方案
- **解决方案**：移除有问题的loom依赖，使用稳定的替代方案

### 🧪 测试和验证

#### 内存安全测试
- 使用AddressSanitizer检测内存泄漏
- 使用ThreadSanitizer检测竞态条件
- 使用Miri检测未定义行为

#### 性能测试
- 基准测试确保修复不影响性能
- 并发测试验证多线程安全性

### 📚 开发指南

#### 编写unsafe代码的准则
1. 必须有充分的理由
2. 必须有详细的safety注释
3. 必须通过code review
4. 必须有对应的测试

#### 资源管理
1. 优先使用RAII模式
2. 避免`mem::forget`
3. 确保异常安全
4. 使用智能指针管理复杂资源

## Known Issues and Limitations

### Sanitizer Compatibility
The project includes memory sanitizer tests (AddressSanitizer, ThreadSanitizer, and Miri) in CI, but these have some known limitations:

- **Miri (Undefined Behavior Detector)**:
  - Does not support io-uring syscalls (syscall 425)
  - Some tests are conditionally skipped under Miri using `#[cfg(not(miri))]`
  - This is expected behavior as Miri is an interpreter that doesn't support all kernel features

- **AddressSanitizer (ASan) + LeakSanitizer (LSan)**:
  - May have compatibility issues with certain dependencies (e.g., `generator` crate build.rs issues)
  - Some proc-macro crates may not be fully compatible with sanitizers
  - CI jobs use `continue-on-error` to prevent blocking the entire workflow

- **ThreadSanitizer (TSan)**:
  - Similar compatibility issues with some dependencies
  - May detect false positives in certain async code patterns

### Workarounds
- For local development, use `make test` for regular testing
- Sanitizer tests are primarily for CI validation and may be skipped locally if dependencies have issues
- The core functionality remains unaffected by these sanitizer limitations

### Performance Benchmarking
- HTTP performance comparisons with tokio are available via `scripts/bench_http.sh`
- Cross-CPU latency benchmarks are included in the benches directory
- Scheduling throughput benchmarks test task spawning performance

## Status

This is a learning project and work in progress. See [DEVELOPMENT_PLAN.md](DEVELOPMENT_PLAN.md) for the roadmap.

- Project purpose and usage notes: see [docs/USAGE_AND_POSITIONING.md](docs/USAGE_AND_POSITIONING.md)

## License

MIT OR Apache-2.0
