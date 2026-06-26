//! True zero-copy HTTP echo using read_rent / write_rent.
//!
//! The key insight: read data stays in the Buffer the kernel wrote into.
//! No memcpy of payload at any point:
//!   1. read_rent → kernel fills user's Buffer directly (zero copy)
//!   2. write_rent(header) → small header from a separate Buffer
//!   3. write_rent(body) → the original read Buffer goes straight to kernel (zero copy)
//!
//! HTTP pipelining over a single TCP connection works because TCP
//! guarantees ordering: header bytes arrive before body bytes.

use rust_miniss::net::AsyncTcpListener;
use rust_miniss::{Buffer, MultiCoreRuntime};

use std::io;
use std::net::SocketAddr;

async fn handle_client(stream: rust_miniss::net::AsyncTcpStream) {
    // Single buffer per connection, reused across the entire lifecycle.
    let mut buf = Buffer::with_capacity(8192);

    loop {
        match stream.read_rent(buf).await {
            Ok((0, _consumed)) => break,
            Ok((n, read_buf)) => {
                // Build HTTP header (small, unavoidable allocation for text).
                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n");
                let header_buf = Buffer::from_slice(header.as_bytes());

                // Write header. On error, the connection is dead.
                if stream.write_rent(header_buf).await.is_err() {
                    break;
                }

                // Write body directly — zero copy of the payload.
                // The returned buffer is reused for the next read.
                match stream.write_rent(read_buf).await {
                    Ok((_, returned_buf)) => buf = returned_buf,
                    Err(_) => break,
                }
            }
            Err(_) => break,
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

    let runtime = MultiCoreRuntime::new(Some(1))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let addr: SocketAddr = addr.parse().expect("Invalid address");

    let rt = runtime.clone();
    runtime
        .spawn(async move {
            let listener = match AsyncTcpListener::bind(addr) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("Failed to bind to {addr}: {e}");
                    return;
                }
            };

            loop {
                match listener.accept().await {
                    Ok((stream, _client_addr)) => {
                        let _ = rt.spawn(handle_client(stream));
                    }
                    Err(e) => {
                        eprintln!("Accept error: {e}");
                        break;
                    }
                }
            }
        })
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    std::thread::park();
    Ok(())
}
