//! compio HTTP echo server for runtime comparison.
//! Supports --cores N with SO_REUSEPORT for multi-core accept.

use compio::io::AsyncWriteExt;
use compio::net::{TcpListener, TcpStream};
use compio_io::AsyncRead;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::available_parallelism;
use std::time::Duration;

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

async fn handle_client(mut stream: TcpStream, requests: Option<Arc<AtomicU64>>) {
    let mut buf = vec![0u8; 4096];
    let mut framer = RequestFramer::new();
    let body = b"Hello from rust-miniss";
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
            Ok(bytes_read) => {
                let complete = match framer.push(&buf[..bytes_read]) {
                    Ok(complete) => complete,
                    Err(()) => break,
                };
                for _ in 0..complete {
                    if let Some(counter) = &requests {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    if stream.write_all(response.clone()).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn run_worker(addr: SocketAddr, core_id: usize, requests: Option<Arc<AtomicU64>>) {
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
                    let requests = requests.clone();
                    compio::runtime::spawn(async move {
                        handle_client(stream, requests).await;
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

fn spawn_metrics_reporter(label: &'static str, requests: Vec<Arc<AtomicU64>>) {
    if let Some(interval_ms) = std::env::var("MINISS_METRICS_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|interval_ms| *interval_ms > 0)
    {
        std::thread::spawn(move || {
            let interval = Duration::from_millis(interval_ms);
            loop {
                std::thread::sleep(interval);
                let mut total = 0u64;
                let mut per_worker = Vec::with_capacity(requests.len());
                for counter in &requests {
                    let value = counter.load(Ordering::Relaxed);
                    total += value;
                    per_worker.push(value);
                }
                eprintln!(
                    "{label}.metrics requests={} per_worker={per_worker:?}",
                    total
                );
            }
        });
    }
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

    let request_metrics_enabled = std::env::var("MINISS_REQUEST_METRICS")
        .ok()
        .map(|value| value == "1")
        .unwrap_or(false);
    let requests: Vec<_> = if request_metrics_enabled {
        (0..cores).map(|_| Arc::new(AtomicU64::new(0))).collect()
    } else {
        Vec::new()
    };
    if request_metrics_enabled {
        spawn_metrics_reporter("compio", requests.clone());
    }

    let mut handles = Vec::with_capacity(cores);
    for i in 0..cores {
        let requests = if request_metrics_enabled {
            Some(Arc::clone(&requests[i]))
        } else {
            None
        };
        handles.push(std::thread::spawn(move || run_worker(addr, i, requests)));
    }

    for h in handles {
        h.join().unwrap();
    }
}
