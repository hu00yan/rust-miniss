//! Single-threaded HTTP echo server for fair benchmarking against glommio/compio/monoio.
//! Runs everything on one thread — no cross-core message passing.

use futures_util::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::executor::Runtime;
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};

use std::io;
use std::io::Write as _;
use std::net::SocketAddr;

async fn handle_client(mut stream: AsyncTcpStream) {
    let mut buf = vec![0u8; 4096];
    let mut response = Vec::with_capacity(8192);
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(bytes_read) => {
                response.clear();
                let _ = write!(
                    response,
                    "HTTP/1.1 200 OK\r\nContent-Length: {bytes_read}\r\n\r\n"
                );
                response.extend_from_slice(&buf[..bytes_read]);
                if stream.write_all(&response).await.is_err() {
                    break;
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

    let runtime = Runtime::new();
    let addr: SocketAddr = addr.parse().expect("Invalid address");

    runtime.block_on(async {
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
                    runtime.spawn(async move {
                        handle_client(stream).await;
                    });
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
