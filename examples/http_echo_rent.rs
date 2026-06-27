//! Owned-buffer HTTP echo using read_rent and vectored writes.
//!
//! The key insight: read data stays in the Buffer the kernel wrote into.
//! The response uses one writev submission for header + body:
//!   1. read_rent → kernel fills user's Buffer directly (zero copy)
//!   2. write_vectored_rent(header, body) → one kernel writev operation
//!
//! HTTP pipelining over a single TCP connection works because TCP
//! guarantees ordering: header bytes arrive before body bytes.

use futures::io::AsyncWriteExt;
use rust_miniss::net::AsyncTcpListener;
use rust_miniss::{Buffer, MultiCoreRuntime};

use std::io;
use std::io::Write as _;
use std::net::SocketAddr;

async fn write_response(
    stream: &mut rust_miniss::net::AsyncTcpStream,
    header: Buffer,
    body: Buffer,
) -> io::Result<Buffer> {
    let total_len = header.len() + body.len();
    let (written, mut buffers) = stream.write_vectored_rent(vec![header, body]).await?;

    let header = buffers.remove(0);
    let body = buffers.remove(0);
    if written >= total_len {
        return Ok(body);
    }

    if written < header.len() {
        stream.write_all(&header[written..]).await?;
        stream.write_all(&body).await?;
    } else {
        let body_offset = written - header.len();
        stream.write_all(&body[body_offset..]).await?;
    }

    Ok(body)
}

async fn handle_client(mut stream: rust_miniss::net::AsyncTcpStream) {
    // Single buffer per connection, reused across the entire lifecycle.
    let mut buf = Buffer::with_capacity(8192);
    let mut header = Vec::with_capacity(64);

    loop {
        match stream.read_rent(buf).await {
            Ok((0, _consumed)) => break,
            Ok((n, read_buf)) => {
                header.clear();
                let _ = write!(header, "HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n");
                match write_response(&mut stream, Buffer::from_slice(&header), read_buf).await {
                    Ok(returned_body) => buf = returned_body,
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

    let runtime = MultiCoreRuntime::new(Some(1)).map_err(|e| io::Error::other(e.to_string()))?;
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
        .map_err(|e| io::Error::other(e.to_string()))?;

    std::thread::park();
    Ok(())
}
