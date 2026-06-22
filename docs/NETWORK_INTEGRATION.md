# Network Integration Guide

This document covers TCP, UDP, HTTP, and QUIC integration in rust-miniss.

## Overview

rust-miniss provides a thread-per-core async runtime with multiple I/O backends:
- **io-uring**: High-performance completion-based I/O on Linux 5.10+
- **epoll**: Readiness-based I/O on Linux and other Unix systems
- **kqueue**: macOS support via mio

The runtime supports HTTP/1.1, HTTP/2, and HTTP/3 through integration with the hyper and quinn ecosystems.

## TCP

### Basic Usage

```rust
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};

let listener = AsyncTcpListener::bind("127.0.0.1:8080")?;
let (stream, addr) = listener.accept().await?;
```

### Features

- Non-blocking I/O with async/await
- Supports both io-uring and epoll backends
- Implements `futures::io::{AsyncRead, AsyncWrite}`
- Panic isolation between tasks

## UDP

### Basic Usage

```rust
use rust_miniss::net::AsyncUdpSocket;

let socket = AsyncUdpSocket::bind("127.0.0.1:8081")?;

// Receive data
let (len, addr) = socket.recv_from(&mut buf).await?;

// Send data
socket.send_to(b"hello", target_addr).await?;
```

### Features

- Non-blocking UDP I/O
- Supports both io-uring and epoll backends
- Works with QUIC protocols

## HTTP/1.1 + HTTP/2 (Hyper Integration)

rust-miniss integrates with the [hyper](https://hyper.rs) ecosystem to provide HTTP/1.1 and HTTP/2 support.

### Setup

Add to `Cargo.toml`:

```toml
[dependencies]
rust-miniss = { version = "0.1", features = ["hyper_integration"] }
hyper = { version = "1", features = ["http1", "http2", "server"] }
http-body-util = "0.1"
bytes = "1"
```

### HTTP/1.1 Server Example

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

### HTTP/2 Server Example

```rust
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use rust_miniss::net::AsyncTcpListener;
use rust_miniss::hyper_compat::{HyperExecutor, MinissIo};

async fn handle(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    Ok(Response::new(Full::new(Bytes::from("Hello from rust-miniss HTTP/2!"))))
}

fn main() {
    let runtime = rust_miniss::MultiCoreRuntime::new(Some(1)).unwrap();
    
    runtime.block_on(async {
        let listener = AsyncTcpListener::bind("0.0.0.0:8081").unwrap();
        
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            HyperExecutor::spawn(async move {
                let io = MinissIo::new(stream);
                hyper::server::conn::http2::Builder::new(HyperExecutor)
                    .serve_connection(io, service_fn(handle))
                    .await
                    .unwrap();
            });
        }
    });
}
```

### Key Types

- **`MinissIo<T>`**: Wrapper that implements `hyper::rt::{Read, Write}` for any `futures::io::{AsyncRead, AsyncWrite}` type
- **`HyperExecutor`**: Executor that spawns tasks using `tokio::spawn`
- **`MinissTimer`**: Timer implementation using rust-miniss's timer wheel

## QUIC (Quinn Integration)

rust-miniss integrates with [quinn](https://github.com/quinn-rs/quinn) to provide QUIC protocol support.

### Setup

Add to `Cargo.toml`:

```toml
[dependencies]
rust-miniss = { version = "0.1" }
quinn = { version = "0.11", features = ["rustls-ring"] }
rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }
webpki-roots = "0.26"
```

### Basic QUIC Endpoint

```rust
use rust_miniss::quinn_compat::MinissQuinnRuntime;
use std::sync::Arc;

fn main() {
    let runtime = rust_miniss::MultiCoreRuntime::new(Some(1)).unwrap();
    
    runtime.block_on(async {
        // Create TLS configuration
        let mut tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![], todo!())
            .unwrap();
        tls_config.alpn_protocols = vec![b"h3".to_vec()];
        
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap(),
        ));
        
        let endpoint = quinn::Endpoint::server(
            server_config,
            "0.0.0.0:4433".parse().unwrap(),
        ).unwrap();
        
        while let Some(conn) = endpoint.accept().await {
            let conn = conn.await.unwrap();
            // Handle QUIC connection
        }
    });
}
```

### Key Types

- **`MinissQuinnRuntime`**: Runtime implementation for quinn
- **`MinissUdpSocket`**: UDP socket wrapper implementing `quinn::AsyncUdpSocket`
- **`MinissQuinnTimer`**: Timer implementation for quinn

## HTTP/3 (h3 + Quinn)

HTTP/3 is built on top of QUIC. Use the h3 crate with quinn for HTTP/3 support.

### Setup

Add to `Cargo.toml`:

```toml
[dependencies]
rust-miniss = { version = "0.1" }
quinn = { version = "0.11", features = ["rustls-ring"] }
h3 = "0.0.8"
h3-quinn = "0.0.10"
rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }
```

### HTTP/3 Server Example

```rust
use bytes::Bytes;
use h3::server::Connection;
use h3_quinn::Connection as QuinnConnection;
use rust_miniss::quinn_compat::MinissQuinnRuntime;
use std::sync::Arc;

fn main() {
    let runtime = rust_miniss::MultiCoreRuntime::new(Some(1)).unwrap();
    
    runtime.block_on(async {
        // Setup TLS (same as QUIC example)
        let endpoint = setup_quic_endpoint().unwrap();
        
        while let Some(conn) = endpoint.accept().await {
            tokio::spawn(async move {
                let conn = conn.await.unwrap();
                let mut h3_conn = Connection::new(QuinnConnection::new(conn)).await.unwrap();
                
                while let Ok(Some(resolver)) = h3_conn.accept().await {
                    let (req, mut stream) = resolver.resolve_request().await.unwrap();
                    // Handle HTTP/3 request
                }
            });
        }
    });
}
```

## Performance Considerations

### Thread-Per-Core Architecture

rust-miniss uses a thread-per-core model where each CPU core runs independently:
- No lock contention between cores
- Each core has its own event loop, task queue, and timer wheel
- Cross-core communication via lock-free SPSC queues

### I/O Backend Selection

| Backend | Platform | Use Case |
|---------|----------|----------|
| io-uring | Linux 5.10+ | Best performance for high-throughput workloads |
| epoll | Linux, Unix | Fallback for older kernels |
| kqueue | macOS | macOS support |

### QUIC Performance

QUIC (HTTP/3) performs best on:
- Mobile networks with high latency
- Networks with packet loss
- Sub-500 Mbps connections

On high-bandwidth networks (>500 Mbps), HTTP/2 may outperform HTTP/3 due to UDP GRO limitations.

## Examples

See the `examples/` directory for complete working examples:

- `http1_server.rs` - HTTP/1.1 server
- `http2_server.rs` - HTTP/2 server
- `http3_server.rs` - HTTP/3 server with QUIC
- `http_echo.rs` - Simple HTTP echo server

Run examples with:

```bash
cargo run --example http1_server
cargo run --example http2_server
cargo run --example http3_server
```

## Troubleshooting

### Compilation Errors

If you encounter TLS-related errors, ensure the `tls` feature is enabled:

```toml
[dependencies]
rust-miniss = { version = "0.1", features = ["tls"] }
```

### QUIC Connection Issues

- Ensure your TLS certificate is valid
- Check that UDP ports are not blocked by firewalls
- Verify ALPN protocol negotiation includes `h3`

### Performance Issues

- Use `MultiCoreRuntime` with appropriate core count
- Enable io-uring on Linux 5.10+ for best performance
- Consider CPU pinning for consistent performance
