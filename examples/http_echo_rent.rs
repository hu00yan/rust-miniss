//! Owned-buffer HTTP echo using read_rent and static writes.
//!
//! The connection keeps one owned buffer alive and hands it back and forth
//! between user space and the kernel.

use rust_miniss::net::AsyncTcpListener;
use rust_miniss::MultiCoreRuntime;

use std::io;
use std::net::SocketAddr;

static RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nConnection: keep-alive\r\n\r\nHello from rust-miniss";

async fn handle_client(stream: rust_miniss::net::AsyncTcpStream) {
    let mut buf = Vec::with_capacity(8192);

    loop {
        match stream.read_rent(buf).await {
            Ok((0, _)) | Err(_) => break,
            Ok((_, owned_buf)) => {
                if stream.write_static_all(RESPONSE).await.is_err() {
                    break;
                }
                buf = owned_buf;
            }
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
            let mut listener = match AsyncTcpListener::bind(addr) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("Failed to bind to {addr}: {e}");
                    return;
                }
            };

            loop {
                match listener.accept().await {
                    Ok((stream, _client_addr)) => {
                        let _ = rt.spawn_local(handle_client(stream));
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
