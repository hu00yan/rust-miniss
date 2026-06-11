# rust-miniss

[![CI](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml)
[![Docs](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml)
[![Benchmarks](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml)
[![Nightly](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml)
[![Sanitizers](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml)
[![Diff Coverage](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml)
[![Typos](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml)
[![Container Image](https://img.shields.io/badge/ghcr.io-rust--miniss-blue?logo=docker)](https://github.com/hu00yan/rust-miniss/pkgs/container/rust-miniss)

一个最小的线程每核异步运行时，最初受 [Seastar](https://github.com/scylladb/seastar) 启发，但已演变为 Rust 特定的实现。

## 概览

rust-miniss 是一个最小化的异步运行时，展示了高性能服务器设计原理：

- **线程每核架构**：每个 CPU 核心独立运行自己的事件循环
- **无锁通信**：通过 SPSC 队列进行跨 CPU 消息传递
- **多后端 I/O**：io-uring (Linux 5.10+)、epoll (Linux/Unix)、kqueue (macOS)
- **futures::io 兼容性**：实现 `AsyncRead`/`AsyncWrite` traits 以支持生态系统集成
- **Panic 隔离**：任务 panic 不会影响其他任务

## 快速开始

### 基本用法

```rust
use rust_miniss::Runtime;

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        println!("Hello from rust-miniss!");
    });
}
```

### 多核用法

```rust
use rust_miniss::multicore::MultiCoreRuntime;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = MultiCoreRuntime::new(Some(4))?; // 4 个 CPU 核心
    
    runtime.spawn(async {
        println!("任务在 CPU 核心上运行");
    })?;
    
    runtime.shutdown()?;
    runtime.join()?;
    Ok(())
}
```

### 定时器工具

```rust
use rust_miniss::{timer, Runtime};
use std::time::Duration;

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        // 休眠 1 秒
        timer::sleep(Duration::from_secs(1)).await;
        
        // 对操作应用超时
        let result = timer::timeout(Duration::from_secs(2), async {
            timer::sleep(Duration::from_millis(500)).await;
            "操作完成"
        }).await;
        
        match result {
            Ok(value) => println!("成功: {}", value),
            Err(_) => println!("操作超时"),
        }
        
        // 创建周期性间隔
        let mut interval = timer::Interval::new(Duration::from_millis(200));
        for i in 0..3 {
            interval.tick().await;
            println!("滴答 {}", i + 1);
        }
    });
}
```

## 架构

```
┌─────────────────────────────────────────────────────────────────┐
│                    多核运行时                                    │
├─────────────┬─────────────┬─────────────┬─────────────────────┤
│   CPU 0     │   CPU 1     │   CPU 2     │   信号处理器          │
│             │             │             │                     │
│ ┌─────────┐ │ ┌─────────┐ │ ┌─────────┐ │ ┌─────────────────┐ │
│ │任务队列  │ │ │任务队列  │ │ │任务队列  │ │ │OS 信号          │ │
│ │         │ │ │         │ │ │         │ │ │监听器           │ │
│ └─────────┘ │ └─────────┘ │ └─────────┘ │ └─────────────────┘ │
│             │             │             │                     │
│ ┌─────────┐ │ ┌─────────┐ │ ┌─────────┐ │ ┌─────────────────┐ │
│ │定时器    │ │ │定时器    │ │ │定时器    │ │ │跨 CPU           │ │
│ │轮       │ │ │轮       │ │ │轮       │ │ │消息总线         │ │
│ └─────────┘ │ └─────────┘ │ └─────────┘ │ └─────────────────┘ │
│             │             │             │                     │
│ ┌─────────┐ │ ┌─────────┐ │ ┌─────────┐ │                     │
│ │I/O      │ │ │I/O      │ │ │I/O      │ │                     │
│ │后端     │ │ │后端     │ │ │后端     │ │                     │
│ │(uring/  │ │ │(uring/  │ │ │(uring/  │ │                     │
│ │epoll/   │ │ │epoll/   │ │ │epoll/   │ │                     │
│ │kqueue)  │ │ │kqueue)  │ │ │kqueue)  │ │                     │
│ └─────────┘ │ └─────────┘ │ └─────────┘ │                     │
└─────────────┴─────────────┴─────────────┴─────────────────────┘
     SPSC 队列          SPSC 队列
```

## 构建

```bash
# 在 Linux 上使用 io-uring 支持（推荐性能最佳）
cargo build --features io_uring

# 在 macOS 上（通过 mio 使用 kqueue）
cargo build

# 在 Linux 上使用 epoll（默认）
cargo build --features epoll
```

## I/O 后端选择

运行时根据平台和内核版本自动选择最佳 I/O 后端：

| 平台 | 后端 | 特性标志 | 备注 |
|------|------|----------|------|
| Linux 5.10+ | io-uring | `io_uring` | 最佳性能 |
| Linux (旧版) | epoll | `epoll` | Linux 默认 |
| macOS | kqueue | `kqueue` | 通过 mio |
| 其他 Unix | epoll | `epoll` | 备选方案 |

## 示例

- `examples/hello_world.rs` – 单线程运行时入门
- `examples/http_echo.rs` – HTTP/1.1 回声服务器
- `examples/udp_buffered.rs` – 带缓冲 I/O 的 UDP 套接字
- `examples/metrics_server.rs` – Prometheus 指标导出器

### HTTP 回声服务器

```bash
# 构建并运行
cargo build --release --example http_echo
./target/release/examples/http_echo --addr 0.0.0.0:8080

# 使用 curl 测试
curl http://127.0.0.1:8080

# 使用 wrk 进行基准测试
wrk -t4 -c100 -d30s http://127.0.0.1:8080
```

## 目标

1. **教育性**：了解异步运行时内部工作原理
2. **性能**：在特定用例中实现接近最优的性能
3. **简洁性**：保持代码库小而易懂

## 许可证

MIT OR Apache-2.0