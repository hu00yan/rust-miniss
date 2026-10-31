//! hyper 1.x HTTP echo server on tokio — benchmark CONTROL for the
//! hyper-on-miniss comparison (`examples/http_hyper_echo.rs`).
//!
//! Serves the identical hyper http1::Builder request path as the miniss
//! example, over tokio's TcpListener/TcpStream, with the same 22-byte payload.
//! If this control matches the minimal-framer numbers while
//! hyper-on-miniss collapses, the gap is a miniss×hyper interaction and not
//! hyper's intrinsic cost.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper::Response;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::thread::available_parallelism;
use tokio::net::TcpListener;

const RESPONSE_BODY: &[u8] = b"Hello from rust-miniss";

async fn echo_service(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let response = Response::builder()
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(RESPONSE_BODY)))
        .expect("static echo response is always valid");
    Ok(response)
}

async fn serve_connection(stream: tokio::net::TcpStream) {
    let mut builder = http1::Builder::new();
    builder.keep_alive(true);
    let _ = builder
        .serve_connection(TokioIo::new(stream), service_fn(echo_service))
        .await;
}

fn main() {
    let mut cores = available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut addr = String::from("127.0.0.1:8083");
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
                    cores = v.parse().unwrap_or(cores);
                }
            }
            _ => {}
        }
    }
    let addr: SocketAddr = addr.parse().expect("valid listen address");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores)
        .enable_all()
        .build()
        .expect("tokio runtime");

    runtime.block_on(async move {
        let listener = TcpListener::bind(addr)
            .await
            .expect("hyper-tokio control bind");
        println!(
            "hyper-tokio echo (hyper http1 on tokio) listening on {addr} with {cores} threads"
        );
        loop {
            let (stream, _) = listener.accept().await.expect("hyper-tokio control accept");
            let _ = stream.set_nodelay(true);
            tokio::spawn(serve_connection(stream));
        }
    });
}
