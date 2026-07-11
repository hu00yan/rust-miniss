//! tokio HTTP echo server for runtime comparison.
//! Supports --cores N for multi-threaded worker pool.

use std::thread::available_parallelism;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[path = "../support/http_framing.rs"]
mod http_framing;
use http_framing::RequestFramer;

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
    let mut buf = [0u8; 4096];
    let body = b"Hello from tokio";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let response = [header.as_bytes(), body].concat();
    let mut framer = RequestFramer::new();

    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(bytes_read) => {
                let complete = match framer.push(&buf[..bytes_read]) {
                    Ok(complete) => complete,
                    Err(()) => break,
                };
                for _ in 0..complete {
                    if stream.write_all(&response).await.is_err() {
                        return;
                    }
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

    if cores == 1 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()?;
        rt.block_on(async move {
            let listener = TcpListener::bind(&addr).await.unwrap();
            println!("tokio http_echo listening on {addr} (single-thread)");
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    handle_client(stream).await;
                });
            }
        })
    } else {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cores)
            .enable_io()
            .build()?;
        rt.block_on(async move {
            let listener = TcpListener::bind(&addr).await.unwrap();
            println!("tokio http_echo listening on {addr} with {cores} workers");
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    handle_client(stream).await;
                });
            }
        })
    }
}
