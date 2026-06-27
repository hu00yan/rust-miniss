//! HTTP echo server for benchmarking the miniss runtime.

use rust_miniss::multicore::MultiCoreRuntime;
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Buffer;
use std::io;
use std::net::SocketAddr;

static RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nConnection: keep-alive\r\n\r\nHello from rust-miniss";

async fn handle_client(stream: AsyncTcpStream) {
    let mut buf = Buffer::with_capacity(4096);

    loop {
        match stream.read_rent(buf).await {
            Ok((0, _)) | Err(_) => break,
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
    let mut cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => {
                if let Some(v) = args.next() {
                    addr = v;
                }
            }
            "--cores" | "-c" => {
                if let Some(v) = args.next() {
                    cores = v.parse::<usize>().map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid --cores value {v:?}: {e}"),
                        )
                    })?;
                }
            }
            _ => {}
        }
    }

    if cores == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--cores must be greater than zero",
        ));
    }

    let addr: SocketAddr = addr.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid listen address: {e}"),
        )
    })?;

    let runtime = MultiCoreRuntime::new(Some(cores))
        .map_err(|e| io::Error::other(format!("failed to create runtime: {e}")))?;

    for core_id in 0..cores {
        let worker_runtime = runtime.clone();
        runtime
            .spawn_on(core_id, async move {
                let listener = match AsyncTcpListener::bind_reuse_port(addr) {
                    Ok(listener) => listener,
                    Err(e) => {
                        eprintln!("bind error on core {core_id}: {e}");
                        return;
                    }
                };

                println!("rust-miniss http_echo listening on {addr} with {cores} worker cores");
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            if let Err(e) = worker_runtime.spawn_on(core_id, handle_client(stream))
                            {
                                eprintln!("spawn error on core {core_id}: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("accept error on core {core_id}: {e}");
                            break;
                        }
                    }
                }
            })
            .map_err(|e| io::Error::other(format!("failed to spawn accept loop: {e}")))?;
    }

    runtime.block_on(futures::future::pending::<()>());
    let _ = runtime.shutdown();
    Ok(())
}
