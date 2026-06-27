//! compio HTTP echo server for runtime comparison.
//! Supports --cores N with SO_REUSEPORT for multi-core accept.

use compio::io::AsyncWriteExt;
use compio::net::{TcpListener, TcpStream};
use compio_io::AsyncRead;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, IntoRawFd};
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
    let mut buf = vec![0u8; 4096];
    let body = b"Hello from compio";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let response = [header.as_bytes(), body].concat();

    loop {
        let read_result = stream.read(buf).await;
        buf = read_result.1;
        match read_result.0 {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if stream.write_all(response.clone()).await.is_err() {
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

    let fd = socket.into_raw_fd();
    // Safety: fd is valid and unique
    let listener = unsafe { TcpListener::from_raw_fd(fd) };

    let runtime = compio::runtime::Runtime::new().unwrap();
    runtime.block_on(async move {
        println!("compio worker {core_id} listening on {addr}");
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    compio::runtime::spawn(async move {
                        handle_client(stream).await;
                    })
                    .detach();
                }
                Err(e) => {
                    eprintln!("compio worker {core_id} accept error: {e}");
                    break;
                }
            }
        }
    });
}

fn main() {
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

    let addr: SocketAddr = addr.parse().expect("invalid address");

    println!("compio http_echo listening on {addr} with {cores} workers");

    let mut handles = Vec::with_capacity(cores);
    for i in 0..cores {
        handles.push(std::thread::spawn(move || run_worker(addr, i)));
    }

    for h in handles {
        h.join().unwrap();
    }
}
