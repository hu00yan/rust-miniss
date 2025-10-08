//! An asynchronous HTTP echo server using the miniss runtime.
//! For production HTTP, integrate hyper:
//! Add to Cargo.toml: hyper = { version = "1", features = ["http1", "server", "tcp"] }
//! Then use hyper's Server with miniss-compatible streams.

use rust_miniss::{
    net::{AsyncTcpListener, AsyncTcpStream},
    task, Runtime,
};
use futures_util::io::{AsyncReadExt, AsyncWriteExt};

use std::io;
use std::net::SocketAddr;

async fn handle_client(mut stream: AsyncTcpStream) {
    println!("📡 Handling HTTP client");

    // Read from the stream
    let mut buf = vec![0u8; 4096];
    match stream.read(&mut buf).await {
        Ok(bytes_read) => {
            println!("   Received {} bytes", bytes_read);
            if let Ok(request) = std::str::from_utf8(&buf[..bytes_read]) {
                println!(
                    "   Request preview: {}",
                    request.lines().next().unwrap_or("")
                );
            }

            // Echo back the request as response
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                bytes_read, std::str::from_utf8(&buf[..bytes_read]).unwrap_or("")
            );

            if let Err(e) = stream.write_all(response.as_bytes()).await {
                eprintln!("   Failed to write response: {}", e);
            }
        }
        Err(e) => {
            eprintln!("   Failed to read from stream: {}", e);
        }
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

    println!("🚀 Starting HTTP Echo Server");
    println!("Server will listen on http://{}", addr);
    println!("Try: curl http://{}", addr);

    let runtime = Runtime::new();
    let addr: SocketAddr = addr.parse().unwrap();

    runtime.block_on(async {
        let listener = match AsyncTcpListener::bind(addr) {
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

                    // Spawn each client connection as a separate task for concurrency
                    if let Err(e) = task::spawn(handle_client(stream)) {
                        eprintln!("Failed to spawn client handler: {}", e);
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

// Example: Integrating hyper for full HTTP/1.1 support
// Add to Cargo.toml: hyper = { version = "1", features = ["http1", "server"] }
// Then use tokio_util::compat::Compat to bridge miniss streams to Tokio
//
// async fn run_hyper_server() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//     use hyper::{Body, Request, Response, Server};
//     use hyper::service::service_fn;
//     use std::convert::Infallible;
//     use tokio_util::compat::TokioAsyncReadCompatExt;
//
//     let addr = ([0, 0, 0, 0], 8080).into();
//     let listener = AsyncTcpListener::bind(addr)?;
//     let service = service_fn(|_req: Request<Body>| async {
//         Ok::<_, Infallible>(Response::new(Body::from("Hello from hyper + miniss!")))
//     });
//
//     // Bridge miniss stream to Tokio-compatible stream
//     loop {
//         let (stream, _) = listener.accept().await?;
//         let tokio_stream = TokioAsyncReadCompatExt::compat(stream);
//         // Then use hyper with tokio_stream
//     }
//     Ok(())
// }

#[allow(dead_code)]
async fn run_server() -> io::Result<()> {
    let addr_str = "0.0.0.0:8080";
    let addr: SocketAddr = addr_str.parse().expect("Failed to parse address");
    let listener = AsyncTcpListener::bind(addr)?;
    println!("HTTP echo server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, client_addr)) => {
                if let Some(addr) = client_addr {
                    println!("Accepted connection from: {}", addr);
                } else {
                    println!("Accepted connection from: <unknown>");
                }
                if let Err(e) = task::spawn(handle_client(stream)) {
                    eprintln!("Failed to spawn task: {}", e);
                }
            }
            Err(e) => {
                eprintln!("Failed to accept connection: {}", e);
            }
        }
    }
}
