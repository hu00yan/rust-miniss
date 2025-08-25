//! An asynchronous HTTP echo server using the miniss runtime.

use rust_miniss::{
    multicore,
    net::{AsyncTcpListener, AsyncTcpStream},
    task,
};
use std::io;
use std::net::SocketAddr;

async fn handle_client(stream: AsyncTcpStream) {
    // Read from the stream
    if let Err(e) = stream.read().await {
        eprintln!("Failed to read from stream: {}", e);
        return;
    }

    // A minimal, static HTTP response
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nHello world!";

    // Write the response
    if let Err(e) = stream.write_all(response).await {
        eprintln!("Failed to write to stream: {}", e);
    }
}

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

fn main() {
    // Initialize the multi-core runtime with the optimal number of CPUs.
    multicore::init_runtime(None).expect("Failed to initialize runtime");

    // Block on the main server future.
    if let Err(e) = multicore::block_on(run_server()) {
        eprintln!("Server error: {}", e);
    }
}
