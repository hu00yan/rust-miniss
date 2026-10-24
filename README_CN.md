# rust-miniss

一个最小的线程每核异步运行时，最初受 [Seastar](https://github.com/scylladb/seastar) 启发，但已演变为 Rust 特定的实现。

## 概览

rust-miniss 是一个最小化的异步运行时，展示了高性能服务器设计原理：

- **线程每核架构**：每个 CPU 核心独立运行自己的事件循环
- **有界跨核心通信**：新任务、取消、指标采样和 I/O 资源回收通过显式有界消息或唤醒路径处理；已接收任务不会被偷取或迁移。
- **I/O 后端**：Linux 6.12+ 默认使用 io_uring；较旧内核或探测失败时回退到 mio/epoll；macOS 通过 mio 使用 kqueue。
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
     有界跨核心消息      有界跨核心消息
```

## 构建

```bash
# Linux 6.12+ 默认选择 io_uring；旧内核或探测失败时使用 epoll 回退
cargo build --all-features

# 在现代 Linux 上显式检查 epoll 兼容路径（正确性验证，不是性能目标）
MINISS_IO_BACKEND=epoll cargo check --all-targets --all-features

# 在 macOS 上通过 mio 使用 kqueue
cargo build --all-features
```

## I/O 后端选择

`build.rs` 在编译时依据目标平台和可探测的内核版本选择后端，不是运行时探测。Linux 也可用 `MINISS_IO_BACKEND` 显式覆盖；选择结果必须适用于目标操作系统。`uname` 读取的是构建机内核，交叉编译时请显式指定目标可用的后端。

| 平台 / 条件 | 后端 | 用途 |
|------|------|------|
| Linux（内核 6.12+ 且探测成功） | io_uring | 性能路径 |
| Linux（低于 6.12 或探测失败） | mio / epoll | 兼容与正确性路径 |
| macOS | mio / kqueue | 正确性与开发路径 |

## 示例

- `examples/hello_world.rs` – 单线程运行时入门
- `examples/http_echo.rs` – HTTP/1.1 回声服务器
- `examples/udp_buffered.rs` – 带缓冲 I/O 的 UDP 套接字

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

## 测试状态

当前 macOS/kqueue 与 Linux/io-uring 的完整 feature 测试、HTTP/2、TLS、QUIC/H3、
UDP、多核生命周期和 Loom 取消模型均有自动化回归。TLS 根证书平台矩阵、持续多核
QUIC/UDP 压力和长期性能矩阵仍未完成；这些状态以 `TODO.md` 为准。

完整正确性套件入口是 `just test-full`。它会串行运行格式/静态检查、feature 编译
矩阵、一次全 feature 普通回归、差分模型、Loom 和压力测试；Linux 内核 6.12+ 且选择 io_uring 时会运行 io_uring safety，旧内核使用 epoll 时该 gate 会明确打印 `SKIP`。检测到 nightly
和 Miri 时运行 Miri。被当前主机跳过的 gate 会明确打印 `SKIP`。

## 与 monoio/compio 的当前差距

除了性能，当前差距主要在成熟度和 API 范围，不在核心调度模型：

- 和 monoio、compio 一样，`rust-miniss` 的数据面采用 thread-per-core，
  不是 Tokio 那种 work-stealing 加透明任务迁移模型。若有跨线程 dispatch 或
  blocking offload，那是显式的操作，不是把已有任务偷偷迁移到另一个核心；
- 公开 API、生态适配、文档、发布版本和生产案例都比成熟运行时少；
- TLS 当前主要是客户端流，HTTP/2 和 HTTP/3 分别通过 Hyper、Quinn/H3 集成，
  不是完整的协议框架；
- 明确支持 Linux 和 macOS；较新的 Linux 内核可使用 io_uring 性能路径，旧内核通过 mio/epoll 提供兼容正确性路径。

这些是项目当前的产品边界。`IOSQE_IO_LINK`、provided buffer、multishot receive、
registered buffer、SQPOLL 等是 runtime 内部的实现手段，不是必须暴露给用户的功能。
上层只表达功能语义，由 runtime 在不改变 API 行为的前提下选择实现；直接暴露内核
flag 会造成抽象泄露。

## 许可证

MIT OR Apache-2.0
