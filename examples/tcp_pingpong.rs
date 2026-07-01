//! TCP ping-pong benchmark — same as monoio's test methodology.
//! Client sends a small message, server echoes it back.
//! Measures raw I/O + scheduling overhead without HTTP overhead.

use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::MultiCoreRuntime;

use std::io;
use std::net::SocketAddr;

async fn handle_client(stream: AsyncTcpStream) {
    let mut buf = Vec::with_capacity(256);
    loop {
        match stream.read_rent(buf).await {
            Ok((0, _)) | Err(_) => break,
            Ok((_, owned_buf)) => match stream.write_rent(owned_buf).await {
                Ok((_, returned_buf)) => buf = returned_buf,
                Err(_) => break,
            },
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

    let runtime =
        MultiCoreRuntime::new(Some(num_cores)).map_err(|e| io::Error::other(e.to_string()))?;
    let addr: SocketAddr = addr.parse().expect("Invalid address");

    runtime.block_on(async {
        let mut listener = AsyncTcpListener::bind(addr).expect("Failed to bind");
        while let Ok((stream, _)) = listener.accept().await {
            runtime.spawn(handle_client(stream)).ok();
        }
    });
    Ok(())
}
