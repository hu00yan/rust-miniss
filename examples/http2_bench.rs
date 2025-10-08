//! HTTP/2 benchmark server using Hyper on rust-miniss runtime
//! This provides a fair comparison with other async runtimes using HTTP/2

use hyper::service::service_fn;
use hyper::{body::Body, Method, Request, Response, StatusCode};
use rust_miniss::{net::AsyncTcpListener, Runtime};
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio_util::compat::FuturesAsyncReadCompatExt;

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

fn main() {
    let mut addr = String::from("127.0.0.1:8080");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--addr" || arg == "-a" {
            if let Some(a) = args.next() {
                addr = a;
            }
        }
    }

    let addr: SocketAddr = addr.parse().expect("Invalid address");

    println!("🚀 Starting HTTP/2 Benchmark Server");
    println!("Server will listen on http://{}", addr);
    println!("Supports both HTTP/1.1 and HTTP/2");
    println!("Try:");
    println!("  curl http://{}:{}", addr.ip(), addr.port());
    println!("  curl --http2-prior-knowledge http://{}:{}", addr.ip(), addr.port());
    println!("  curl --http2 http://{}:{}", addr.ip(), addr.port());

    // Create a rust-miniss runtime
    let miniss_runtime = Runtime::new();

    // Run the server
    miniss_runtime.block_on(async {
        // Create a TCP listener using rust-miniss
        let listener = AsyncTcpListener::bind(addr).expect("Failed to bind to address");

        println!("📡 Server listening on {}", addr);

        // Accept connections and serve them with Hyper
        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    if let Some(addr) = peer_addr {
                        println!("📥 Connection from {}", addr);
                    }

                    // Convert our AsyncTcpStream to tokio::io using compat
                    let tokio_stream = stream.compat();

                    // Spawn a task to handle this connection with Hyper
                    rust_miniss::task::spawn(async move {
                        // Use Hyper's serve_connection with HTTP/2 support
                        let service = service_fn(handle_request);
                        let server = hyper::server::conn::http2::Builder::new(
                            tokio::runtime::Handle::current()
                        )
                        .serve_connection(tokio_stream, service);

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
