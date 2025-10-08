//! HTTP/2 server for rust-miniss, inspired by glommio's hyper integration

use hyper::service::service_fn;
use hyper::{body::Body, Method, Request, Response, StatusCode};
use rust_miniss::{
    net::AsyncTcpStream,
    task,
    Runtime,
};
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;

// Custom executor for hyper
#[derive(Clone)]
struct MinissExecutor;

impl<F> hyper::rt::Executor<F> for MinissExecutor
where
    F: std::future::Future + 'static,
    F::Output: 'static,
{
    fn execute(&self, fut: F) {
        task::spawn(async move {
            let _ = fut.await;
        }).detach();
    }
}

// Wrapper to implement hyper's Read/Write traits
struct HyperStream(AsyncTcpStream);

impl hyper::rt::Read for HyperStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use futures_util::io::AsyncRead;
        use std::pin::Pin;

        unsafe {
            let read_slice = {
                let buffer = buf.as_mut();
                buffer.as_mut_ptr().write_bytes(0, buffer.len());
                std::slice::from_raw_parts_mut(buffer.as_mut_ptr() as *mut u8, buffer.len())
            };
            Pin::new(&mut self.0).poll_read(cx, read_slice).map(|n| {
                if let Ok(n) = n {
                    buf.advance(n);
                }
                Ok(())
            })
        }
    }
}

impl hyper::rt::Write for HyperStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use futures_util::io::AsyncWrite;
        use std::pin::Pin;

        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<std::io::Result<()>> {
        use futures_util::io::AsyncWrite;
        use std::pin::Pin;

        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<std::io::Result<()>> {
        use futures_util::io::AsyncWrite;
        use std::pin::Pin;

        Pin::new(&mut self.0).poll_close(cx)
    }
}

async fn handle_request(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => Ok(Response::new(Body::from(
            "Hello from HTTP/2 server on rust-miniss!",
        ))),
        (&Method::GET, "/json") => {
            let json = r#"{"message":"Hello","runtime":"rust-miniss","protocol":"HTTP/2"}"#;
            Ok(Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(json))
                .unwrap())
        }
        (&Method::POST, "/echo") => {
            // For benchmark, just return a simple response
            Ok(Response::new(Body::from("Echo response")))
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap()),
    }
}

fn main() -> io::Result<()> {
    let mut addr = String::from("127.0.0.1:8080");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--addr" || arg == "-a" {
            if let Some(v) = args.next() {
                addr = v;
            }
        }
    }

    println!("🚀 Starting HTTP/2 Server");
    println!("Server will listen on http://{}", addr);
    println!("Supports HTTP/1.1 and HTTP/2");
    println!("Try:");
    println!("  curl http://{}", addr);
    println!("  curl --http2-prior-knowledge http://{}", addr);

    let runtime = Runtime::new();
    let addr: SocketAddr = addr.parse().unwrap();

    runtime.block_on(async {
        let listener = match rust_miniss::net::AsyncTcpListener::bind(addr) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Failed to bind to {}: {}", addr, e);
                return;
            }
        };
        println!("📡 Server listening on {}", addr);

        let mut request_count = 0;

        loop {
            match listener.accept().await {
                Ok((stream, client_addr)) => {
                    request_count += 1;
                    println!("📥 Request #{} from {:?}", request_count, client_addr);

                    // Spawn each connection as a separate task
                    let spawn_result = task::spawn(async move {
                        let hyper_stream = HyperStream(stream);
                        let service = service_fn(handle_request);

                        let result = hyper::server::conn::http2::Builder::new(MinissExecutor)
                            .serve_connection(hyper_stream, service)
                            .await;

                        if let Err(e) = result {
                            eprintln!("HTTP/2 connection error: {}", e);
                        }
                    });

                    if let Err(e) = spawn_result {
                        eprintln!("Failed to spawn HTTP/2 handler: {}", e);
                    }
                }
                Err(e) => {
                    eprintln!("❌ Accept error: {}", e);
                    break;
                }
            }
        }
    });

    Ok(())
}