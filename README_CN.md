# rust-miniss

[![CI](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml)
[![Docs](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml)
[![Benchmarks](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml)
[![Nightly](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml)
[![Sanitizers](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml)
[![Diff Coverage](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml)
[![Typos](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml)
[![Container Image](https://img.shields.io/badge/ghcr.io-rust--miniss-blue?logo=docker)](https://github.com/hu00yan/rust-miniss/pkgs/container/rust-miniss)

[miniss](https://github.com/qqiangwu/miniss) 的 Rust 实现 - [Seastar](https://github.com/scylladb/seastar) 框架的简化版本。

## 概览

rust-miniss 是一个最小化的异步运行时，展示了高性能服务器设计原理：

- **无共享架构**：每个 CPU 核心独立运行
- **无锁通信**：通过 SPSC 队列进行跨 CPU 消息传递
- **零拷贝 I/O**：在 Linux 上使用 io-uring
- **自定义 futures**：深入理解异步内部机制
- **🆕 futures::io 支持**：与 Hyper 和 HTTP 生态兼容

## futures::io 集成

rust-miniss 为 `AsyncTcpStream`、`AsyncUnixStream` 和 `BufferedUdpSocket` 实现了 `futures::io::AsyncRead` 和 `AsyncWrite`。这允许与更广泛的异步生态兼容，包括通过 `tokio-util::compat` 与 Hyper 的集成。

### Hyper 示例

```rust
use rust_miniss::{Runtime, net::AsyncTcpStream};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use hyper::client::conn::http1::Builder;
use hyper::service::service_fn;
use hyper::{Body, Request, Response};

let runtime = Runtime::new();
runtime.block_on(async {
    let stream = AsyncTcpStream::connect("127.0.0.1:8080").await.unwrap();
    let compat_stream = stream.compat();  // 转换为 tokio::io::AsyncRead/Write

    let service = service_fn(|_req: Request<Body>| async {
        Ok::<_, std::io::Error>(Response::new(Body::from("Hello from miniss!")))
    });

    let mut http_conn = Builder::new().serve_connection(compat_stream, service).await.unwrap();
    // 处理请求...
});
```

注意：这涉及 memcpy 兼容性权衡（原生 `read()`/`write()` 方法保持零拷贝）。

## UDP 流式处理 (缓冲 I/O)

对于流式 UDP (例如，数据报上的帧协议)，使用 `BufferedUdpSocket` (在 VecDeque 中排队，读取时排水)。

### 示例

```rust
use rust_miniss::{Runtime, net::{AsyncUdpSocket, BufferedUdpSocket}};
use futures::io::BufReader;
use std::net::SocketAddr;

let runtime = Runtime::new();
runtime.block_on(async {
    let socket = AsyncUdpSocket::bind("127.0.0.1:0").unwrap();
    let mut buffered = BufferedUdpSocket::new(socket);
    let peer = "127.0.0.1:9090".parse().unwrap();
    buffered.peer_addr = Some(peer);

    // 接收到缓冲区 (排队数据报)
    buffered.recv_buffered(1024).await.unwrap();

    // 使用 BufReader 进行流读取
    let mut reader = BufReader::new(&buffered[..]);  // 包装用于缓冲操作
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();  // 排水排队的数报

    // 缓冲写入 (排队，flush 发送)
    buffered.write_all(b"response").await.unwrap();
    buffered.flush().await.unwrap();
});
```

警告：丢失数据报边界 (可能部分读取)；对于 DNS 等协议，使用帧 (例如，前缀长度)。

## 向量化 I/O (Scatter-Gather)

TCP/Unix/UDP 的原生 Readv/Writev (最多 8 个 iovec，io-uring opcode)。

### 示例

```rust
use std::io::{IoSlice, IoSliceMut};

let mut bufs = vec![IoSliceMut::new(&mut [0u8; 5]), IoSliceMut::new(&mut [0u8; 9])];
let n = stream.read_vectored(&mut bufs).await.unwrap();  // 直接到切片，部分每个 iovec

let slices = vec![IoSlice::new(b"part1"), IoSlice::new(b"part2")];
let n_write = stream.write_vectored(&slices).await.unwrap();  // 批量发送
```

对于 UDP：批量数据报 (RecvMsg 中的多 iovec)。

## 零拷贝网络

原生 API 零拷贝 (BufferPool from_slice)。对于 traits，使用 Rent (feature="rent-traits")：

### 示例

```rust
#[cfg(feature = "rent-traits")]
use rust_miniss::net::{AsyncReadRent, AsyncWriteRent};

let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
let rented = stream.poll_read_rent(cx, 1024).await.unwrap();  // 返回填充的 Buffer，无拷贝
// 使用 rented (例如，解析)，drop 回收到池
```

回退到 futures::io (兼容的 memcpy)。使用 `--features rent-traits` 启用。

## 内存管理

### 缓冲池 (基于 Slab)

每个核心的 slab 池 (1024 个槽，固定 4KB) 用于重用。get_buffer(len)：分配/重用 slab。recycle_buffer：返回清空的槽。

### 竞技场分配器 (Feature "arena")

Bumpalo 用于 HTTP 瞬态 (每个请求 Bump，alloc_str/slice，结束时重置)。

### 示例 (HTTP)

```rust
#[cfg(feature = "arena")]
use rust_miniss::http::HttpRequestContext;

let bump = Bump::new();
let mut ctx = HttpRequestContext::new(&bump);
ctx.parse_header("Host", "example.com");  // 在竞技场分配
// 处理请求...
bump.reset();  // 释放所有
```

非竞技场使用 Vec。

## 性能监控

使用 `metrics` crate 发出延迟、字节和操作指标 (每个核心的仪表/计数器/直方图)。通过 `metrics-exporter-prometheus` 导出到 Prometheus。

### 设置

在 main 中：

```rust
use metrics_exporter_prometheus::PrometheusBuilder;

let builder = PrometheusBuilder::new();
builder.install_recorder().expect("Failed to install metrics recorder");
```

暴露 `/metrics` (示例在 `examples/metrics_server.rs`)：

```rust
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use std::convert::Infallible;
use std::net::SocketAddr;
use metrics_exporter_prometheus::Encoder;

async fn metrics_handler(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let encoder = Encoder::new();
    let families = metrics::gather();  // 收集所有
    let mut buffer = Vec::new();
    encoder.encode(&families, &mut buffer).map_err(|_| Infallible)?;
    Ok(Response::new(Body::from(buffer)))
}

let addr = SocketAddr::from(([127, 0, 0, 1], 9090));
let make_svc = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(metrics_handler)) });
let server = Server::bind(&addr).serve(make_svc);
tokio::spawn(server);  // 与运行时并行运行
```

使用 Prometheus 抓取 (prometheus.yml)：

```yaml
scrape_configs:
  - job_name: 'miniss'
    static_configs:
      - targets: ['localhost:9090']
    scrape_interval: 15s
```

指标包括：
- `net.read_latency` (直方图，秒)
- `net.write_latency` (直方图，秒)
- `net.read_bytes_total` (计数器)
- `cpu.utilization` (仪表，任务/容量)
- `pool.hits_total` (计数器，slab 重用)
- `udp.*` 缓冲 UDP 的变体。

## 调试工具

Feature "debug"：Tracing spans (io.submit, cpu.tick)。GDB 打印器 (.gdbinit 用于 Buffer)。Valgrind 抑制 (valgrind.supp 用于 uring/epoll)。

### 示例 Tracing

```bash
RUST_LOG=debug cargo run --features debug
# 输出： [DEBUG io_submit] Op::Read fd=3 len=4096
```

GDB：`gdb ./target/debug/rust-miniss`，`print buffer` 显示 len/cap/内容。

## 示例

查看 `examples/` 以获取缓冲 UDP，向量化 I/O，Rent 零拷贝，竞技场 HTTP，完整指标服务器。

## 构建

`cargo build --features arena,advanced,debug` 以启用完整功能。

## 许可证

MIT OR Apache-2.0