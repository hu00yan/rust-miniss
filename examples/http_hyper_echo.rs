//! HTTP/1.1 echo server on hyper 1.x over the miniss runtime.
//!
//! This is the flagship-path counterpart of `examples/http_echo.rs`: instead of
//! the minimal self-written request framer, every request goes through
//! `hyper::server::conn::http1::Builder::serve_connection` running on miniss
//! `AsyncTcpStream` (which implements `hyper::rt::Read`/`hyper::rt::Write`
//! directly, see `src/net.rs`).
//!
//! Architecture contract notes:
//! - one worker thread per core, one SO_REUSEPORT listener per worker
//!   (kernel-dispatch model, same as `http_echo.rs`); accepted connections are
//!   handled with `spawn_local` on the accepting shard, so the hot path adds no
//!   shared state, no locks, no cross-core traffic beyond what hyper itself
//!   needs;
//! - hyper h2-style background task spawning is not used here (plain h1);
//!   `http_hyper_h2_echo.rs` shows the executor wiring for that.
//!
//! Usage: `http_hyper_echo [--addr 127.0.0.1:8080] [--cores N]`

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use rust_miniss::multicore::MultiCoreRuntime;
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;

/// Same 22-byte payload as the minimal-framer `http_echo.rs` response.
const RESPONSE_BODY: &[u8] = b"Hello from rust-miniss";

async fn echo_service(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let response = Response::builder()
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(RESPONSE_BODY)))
        .expect("static echo response is always valid");
    Ok(response)
}

async fn serve_connection(stream: AsyncTcpStream) {
    let mut builder = http1::Builder::new();
    builder.keep_alive(true);
    // Client disconnects and premature closes surface here; the benchmark
    // treats them the same way the minimal framer does (connection over).
    let _ = builder
        .serve_connection(stream, service_fn(echo_service))
        .await;
}

fn main() -> io::Result<()> {
    let mut addr = String::from("127.0.0.1:8080");
    let mut cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => {
                if let Some(v) = args.next() {
                    addr = v;
                }
            }
            "--cores" | "-c" => {
                if let Some(v) = args.next() {
                    cores = v.parse::<usize>().map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid --cores value {v:?}: {e}"),
                        )
                    })?;
                }
            }
            _ => {}
        }
    }

    if cores == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--cores must be greater than zero",
        ));
    }

    let addr: SocketAddr = addr.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid listen address: {e}"),
        )
    })?;

    let runtime = MultiCoreRuntime::new(Some(cores))
        .map_err(|e| io::Error::other(format!("failed to create runtime: {e}")))?;

    let (listener_ready_tx, listener_ready_rx) = std::sync::mpsc::channel();
    for core_id in 0..cores {
        let worker_runtime = runtime.clone();
        let listener_ready_tx = listener_ready_tx.clone();
        runtime
            .spawn_on(core_id, async move {
                let mut listener = match AsyncTcpListener::bind_reuse_port(addr) {
                    Ok(listener) => {
                        let _ = listener_ready_tx.send(Ok(core_id));
                        listener
                    }
                    Err(e) => {
                        let _ = listener_ready_tx
                            .send(Err(format!("bind error on core {core_id}: {e}")));
                        eprintln!("bind error on core {core_id}: {e}");
                        return;
                    }
                };

                println!(
                    "rust-miniss http_hyper_echo (hyper http1) listening on {addr} with {cores} worker cores"
                );
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            // Connection stays on this reuse-port shard for its
                            // whole life; hyper drives it via spawn_local.
                            if let Err(e) = worker_runtime.spawn_local(serve_connection(stream)) {
                                eprintln!("spawn error on core {core_id}: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("accept error on core {core_id}: {e}");
                            break;
                        }
                    }
                }
            })
            .map_err(|e| io::Error::other(format!("failed to spawn accept loop: {e}")))?;
    }
    drop(listener_ready_tx);

    let mut ready_cores = vec![false; cores];
    for _ in 0..cores {
        match listener_ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(core_id)) => ready_cores[core_id] = true,
            Ok(Err(error)) => {
                let _ = runtime.shutdown();
                return Err(io::Error::new(io::ErrorKind::AddrInUse, error));
            }
            Err(error) => {
                let _ = runtime.shutdown();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for all worker listeners: {error}"),
                ));
            }
        }
    }
    if ready_cores.iter().any(|ready| !ready) {
        let _ = runtime.shutdown();
        return Err(io::Error::other("duplicate or missing worker listener ack"));
    }

    runtime.block_on(futures::future::pending::<()>());
    let _ = runtime.shutdown();
    Ok(())
}
