//! HTTP echo server — all work on one core thread, zero cross-thread dispatch.

use rust_miniss::executor::Runtime;
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Buffer;

use std::io;
use std::net::SocketAddr;

static RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nConnection: keep-alive\r\n\r\nHello from rust-miniss";

async fn handle_client(stream: AsyncTcpStream) {
    let mut buf = Buffer::with_capacity(4096);
    loop {
        match stream.read_rent(buf).await {
            Ok((0, _)) => break,
            Err(_) => break,
            Ok((_, returned_buf)) => {
                buf = returned_buf;
                if stream.write_static_all(RESPONSE).await.is_err() {
                    break;
                }
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

    let runtime = Runtime::new();
    let addr: SocketAddr = addr.parse().expect("Invalid address");

    let rt = &runtime;
    runtime.block_on(async move {
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
                    rt.spawn_detached(handle_client(stream));
                }
                Err(e) => {
                    eprintln!("Accept error: {e}");
                    break;
                }
            }
        }
    });

    Ok(())
}
