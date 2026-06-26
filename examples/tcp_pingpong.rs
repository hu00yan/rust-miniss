//! TCP ping-pong benchmark — same as monoio's test methodology.
//! Client sends a small message, server echoes it back.
//! Measures raw I/O + scheduling overhead without HTTP overhead.

use futures_util::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::MultiCoreRuntime;

use std::io;
use std::net::SocketAddr;

async fn handle_client(mut stream: AsyncTcpStream) {
    let mut buf = [0u8; 256];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if stream.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn main() -> io::Result<()> {
    let mut addr = String::from("127.0.0.1:18080");
    let mut num_cores = 1;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => addr = args.next().unwrap(),
            "--cores" | "-n" => num_cores = args.next().unwrap().parse().unwrap(),
            _ => {}
        }
    }

    let runtime = MultiCoreRuntime::new(Some(num_cores))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let addr: SocketAddr = addr.parse().expect("Invalid address");

    runtime.block_on(async {
        let listener = AsyncTcpListener::bind(addr).expect("Failed to bind");
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    runtime.spawn(handle_client(stream)).ok();
                }
                Err(_) => break,
            }
        }
    });
    Ok(())
}
