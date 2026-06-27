//! glommio HTTP echo server for runtime comparison.
//! Supports --cores N via glommio's LocalExecutorPoolBuilder.

use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::net::{TcpListener, TcpStream};
use glommio::PoolPlacement;
use std::thread::available_parallelism;

fn parse_cores() -> usize {
    let mut cores = available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cores" | "-c" => {
                if let Some(v) = args.next() {
                    cores = v.parse().unwrap_or(cores);
                }
            }
            _ => {}
        }
    }
    cores
}

async fn handle_client(mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let mut buf = vec![0u8; 4096];

    let body = b"Hello from glommio";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let response = [header.as_bytes(), body].concat();

    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if stream.write_all(&response).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn main() -> std::io::Result<()> {
    let cores = parse_cores();
    let mut addr = String::from("0.0.0.0:8080");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => {
                if let Some(v) = args.next() {
                    addr = v;
                }
            }
            _ => {}
        }
    }

    println!("glommio http_echo listening on {addr} with {cores} workers");

    if cores == 1 {
        glommio::LocalExecutorBuilder::default()
            .spawn(move || async move {
                let listener = TcpListener::bind(&addr).expect("bind");
                loop {
                    match listener.accept().await {
                        Ok(stream) => {
                            glommio::spawn_local(async move {
                                handle_client(stream).await;
                            })
                            .detach();
                        }
                        Err(e) => {
                            eprintln!("glommio accept error: {e}");
                            break;
                        }
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
        Ok(())
    } else {
        glommio::LocalExecutorPoolBuilder::new(PoolPlacement::MaxSpread(cores, None))
            .on_all_shards(move || async move {
                let listener = TcpListener::bind(&addr).expect("bind");
                loop {
                    match listener.accept().await {
                        Ok(stream) => {
                            glommio::spawn_local(async move {
                                handle_client(stream).await;
                            })
                            .detach();
                        }
                        Err(e) => {
                            eprintln!("glommio accept error: {e}");
                            break;
                        }
                    }
                }
            })
            .unwrap()
            .join_all();
        Ok(())
    }
}
