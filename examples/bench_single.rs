//! Single-threaded echo server benchmark.
//! Uses Runtime (not MultiCoreRuntime) for cleaner measurement.

use futures_util::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::executor::Runtime;
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAYLOAD: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
const CONCURRENCY: usize = 50;
const DURATION_SECS: u64 = 5;

async fn handle_client(mut stream: AsyncTcpStream) {
    let mut buf = vec![0u8; 4096];
    if let Ok(n) = stream.read(&mut buf).await {
        let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {n}\r\n\r\n");
        let mut resp = Vec::with_capacity(header.len() + n);
        resp.extend_from_slice(header.as_bytes());
        resp.extend_from_slice(&buf[..n]);
        let _ = stream.write_all(&resp).await;
    }
}

fn main() {
    let rt = Runtime::new();
    let (port_tx, port_rx) = std::sync::mpsc::channel();

    let server = std::thread::spawn(move || {
        rt.block_on(async {
            let listener =
                AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
            let addr = listener.local_addr().unwrap();
            port_tx.send(addr.port()).unwrap();

            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        rt.spawn(async move { handle_client(stream).await });
                    }
                    Err(e) => {
                        eprintln!("Accept error: {e}");
                        break;
                    }
                }
            }
        });
    });

    let port = port_rx.recv().unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    eprintln!("Server on {addr}");

    // Warmup
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
            let _ = s.write_all(PAYLOAD);
            let mut b = [0; 4096];
            let _ = s.read(&mut b);
        }
    }

    // Benchmark
    let counter = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    eprintln!("Benchmarking {DURATION_SECS}s, {CONCURRENCY} connections...");

    let mut handles = Vec::new();
    for _ in 0..CONCURRENCY {
        let c = counter.clone();
        let d = done.clone();
        let a = addr;
        handles.push(std::thread::spawn(move || loop {
            if d.load(Ordering::Relaxed) {
                break;
            }
            if let Ok(mut s) = std::net::TcpStream::connect_timeout(&a, Duration::from_millis(500))
            {
                if s.write_all(PAYLOAD).is_err() {
                    continue;
                }
                let mut b = [0; 4096];
                if s.read(&mut b).is_ok() {
                    c.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    std::thread::sleep(Duration::from_secs(DURATION_SECS));
    done.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }

    let total = counter.load(Ordering::Relaxed);
    let rps = total as f64 / DURATION_SECS as f64;
    eprintln!("Throughput: {rps:.0} req/s");
}
