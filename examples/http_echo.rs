//! An asynchronous HTTP echo server using the miniss runtime.

use rust_miniss::{
    multicore,
    net::{AsyncTcpListener, AsyncTcpStream},
    task,
};
use std::io;
use std::net::SocketAddr;

async fn handle_client(stream: AsyncTcpStream) {
    println!("📡 Handling HTTP client");

    // Read from the stream
    match stream.read().await {
        Ok((bytes_read, data)) => {
            println!("   Received {} bytes", bytes_read);
            if let Ok(request) = std::str::from_utf8(&data.as_ref()[..bytes_read]) {
                println!(
                    "   Request preview: {}",
                    &request.lines().next().unwrap_or("")
                );
            }
        }
        Err(e) => {
            eprintln!("   Failed to read from stream: {}", e);
            return;
        }
    }

    // A minimal, static HTTP response
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nHello world!";

    // Write the response
    if let Err(e) = stream.write(response).await {
        eprintln!("   Failed to write response: {}", e);
    } else {
        println!("   Sent HTTP response");
    }
}

fn main() -> io::Result<()> {
    println!("🚀 Starting HTTP Echo Server");
    println!("Server will listen on http://127.0.0.1:8080");
    println!("Try: curl http://127.0.0.1:8080");

    let runtime = multicore::MultiCoreRuntime::new(Some(2))
        .map_err(|e| io::Error::other(format!("Runtime error: {}", e)))?;
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    runtime.block_on(async move {
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

                    let _ = task::spawn(async move {
                        handle_client(stream).await;
                    });
                }
                Err(e) => {
                    eprintln!("❌ Accept error: {}", e);
                    break;
                }
            }
        }
    });

    runtime.shutdown().unwrap();
    Ok(())
}

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
