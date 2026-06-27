//! monoio HTTP echo server for runtime comparison.
//! Supports --cores N by spawning N single-threaded runtimes
//! with SO_REUSEPORT for multi-core accept.

use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
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

async fn handle_client(stream: TcpStream) {
    let mut stream = stream;
    let _ = stream.set_nodelay(true);
    let mut buf = vec![0u8; 4096];

    let body = b"Hello from monoio";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let response = [header.as_bytes(), body].concat();

    loop {
        let (result, buf2) = stream.read(buf).await;
        buf = buf2;
        match result {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if stream.write_all(response.clone()).await.0.is_err() {
                    break;
                }
            }
        }
    }
}

fn run_worker(addr: SocketAddr, core_id: usize) {
    // Create socket with SO_REUSEPORT so all workers share the same port
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
        .expect("socket creation failed");
    socket.set_reuse_port(true).expect("set SO_REUSEPORT");
    socket.set_reuse_address(true).expect("set SO_REUSEADDR");
    socket.set_nonblocking(true).expect("set nonblocking");
    socket.bind(&addr.into()).expect("bind failed");
    socket.listen(1024).expect("listen failed");

    let std_listener: std::net::TcpListener = socket.into();

    // monoio 0.2 Runtime is single-thread — one per worker thread.
    // Use IoUringDriver on Linux (FusionDriver needs `fusion` feature).
    let mut rt = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
        .with_entries(1024)
        .build()
        .unwrap();

    rt.block_on(async move {
        let listener = TcpListener::from_std(std_listener).expect("from_std failed");
        println!("monoio worker {core_id} listening on {addr}");
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    monoio::spawn(async move {
                        handle_client(stream).await;
                    });
                }
                Err(e) => {
                    eprintln!("monoio worker {core_id} accept error: {e}");
                    break;
                }
            }
        }
    });
}

fn main() {
    let cores = parse_cores();
    let mut addr = String::from("0.0.0.0:8082");
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

    let addr: SocketAddr = addr.parse().expect("invalid address");

    println!("monoio http_echo listening on {addr} with {cores} workers");

    let handles: Vec<_> = (0..cores)
        .map(|i| std::thread::spawn(move || run_worker(addr, i)))
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}
