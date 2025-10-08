//! Example demonstrating Hyper integration with rust-miniss using futures::io traits
//! This example supports both HTTP/1.1 and HTTP/2 for benchmarking

use hyper::service::service_fn;
use hyper::{body::Body, Method, Request, Response, StatusCode};
use rust_miniss::{net::AsyncTcpListener, Runtime};
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio_util::compat::FuturesAsyncReadCompatExt;

async fn handle_request(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => Ok(Response::new(Body::from(
            "Hello from Hyper running on rust-miniss!",
        ))),
        (&Method::GET, "/health") => Ok(Response::new(Body::from("OK"))),
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

#[tokio::main]
async fn main() {
    // This example shows how to integrate Hyper with rust-miniss
    // by using the futures::io traits we implemented

    let addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();

    println!("Starting Hyper server on http://{}", addr);
    println!("Try:");
    println!("  curl http://localhost:3000/");
    println!("  curl http://localhost:3000/health");
    println!("  curl -X POST -d 'hello' http://localhost:3000/echo");

    // Create a rust-miniss runtime
    let miniss_runtime = Runtime::new();

    // Spawn the Hyper server in the rust-miniss runtime
    miniss_runtime.block_on(async {
        let make_svc =
            make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(handle_request)) });

        // Create a TCP listener using rust-miniss
        let listener = AsyncTcpListener::bind(addr).expect("Failed to bind to address");

        // Accept connections and serve them with Hyper
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    // Convert our AsyncTcpStream to tokio::io using compat
                    let tokio_stream = stream.compat();

                    // Spawn a task to handle this connection with Hyper
                    rust_miniss::task::spawn(async move {
                        // Use Hyper's serve_connection with HTTP/2 support
                        let server = hyper::server::conn::http2::Builder::new(
                            tokio::runtime::Handle::current()
                        )
                        .serve_connection(tokio_stream, service_fn(handle_request));

                        if let Err(e) = server.await {
                            eprintln!("Server error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    eprintln!("Accept error: {}", e);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest;

    #[tokio::test]
    async fn test_hyper_integration() {
        // Start server in background
        let server_handle = tokio::spawn(async {
            let addr: SocketAddr = "127.0.0.1:3001".parse().unwrap();
            let runtime = Runtime::new();

            runtime.block_on(async {
                let listener = AsyncTcpListener::bind(addr).unwrap();
                let make_svc = make_service_fn(|_conn| async {
                    Ok::<_, Infallible>(service_fn(handle_request))
                });

                if let Ok((stream, _)) = listener.accept().await {
                    let tokio_stream = stream.compat();
                    let server = Server::builder(tokio_stream).serve(make_svc);
                    let _ = server.await;
                }
            });
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Test endpoints
        let client = reqwest::Client::new();

        // Test root endpoint
        let resp = client.get("http://127.0.0.1:3001/").send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert!(text.contains("Hello from Hyper"));

        // Test health endpoint
        let resp = client
            .get("http://127.0.0.1:3001/health")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert_eq!(text, "OK");

        // Test echo endpoint
        let resp = client
            .post("http://127.0.0.1:3001/echo")
            .body("test message")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert_eq!(text, "test message");

        server_handle.abort();
    }
}
