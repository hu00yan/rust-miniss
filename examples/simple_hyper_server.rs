//! Simple Hyper server example demonstrating the core integration

use hyper::service::{make_service_fn, service_fn};
use hyper::{body::Body, Request, Response, Server};
use rust_miniss::{net::AsyncTcpListener, Runtime};
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio_util::compat::FuturesAsyncReadCompatExt;

async fn hello_world(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::new(Body::from("Hello, Hyper on rust-miniss!")))
}

fn main() {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    println!("Starting server on http://{}", addr);

    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener = AsyncTcpListener::bind(addr).expect("Failed to bind");

        let make_svc =
            make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(hello_world)) });

        loop {
            if let Ok((stream, _)) = listener.accept().await {
                let tokio_stream = stream.compat();

                rust_miniss::task::spawn(async move {
                    if let Err(e) = Server::builder(tokio_stream).serve(make_svc).await {
                        eprintln!("Connection error: {}", e);
                    }
                });
            }
        }
    });
}
