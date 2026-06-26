//! TCP ping-pong client — 1 thread per connection for max throughput.
//! Usage: tcp_client [--addr 127.0.0.1:18080] [--connections 100] [--duration 10]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Instant;

fn main() {
    let mut addr = "127.0.0.1:18080".to_string();
    let mut num_conns = 100;
    let mut duration_secs = 5;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => addr = args.next().unwrap(),
            "--connections" | "-c" => num_conns = args.next().unwrap().parse().unwrap(),
            "--duration" | "-d" => duration_secs = args.next().unwrap().parse().unwrap(),
            _ => {}
        }
    }

    let msg = b"Hello, world!";

    let start = Instant::now();
    let handles: Vec<_> = (0..num_conns)
        .map(|_| {
            let addr = addr.clone();
            let msg = msg.to_vec();
            thread::spawn(move || {
                let mut conn = loop {
                    match TcpStream::connect(&addr) {
                        Ok(s) => {
                            s.set_nodelay(true).ok();
                            break s;
                        }
                        Err(_) => thread::sleep(std::time::Duration::from_millis(1)),
                    }
                };

                let mut total = 0u64;
                let deadline = Instant::now() + std::time::Duration::from_secs(duration_secs);

                while Instant::now() < deadline {
                    let _ = conn.write_all(&msg);
                    let mut buf = [0u8; 256];
                    let _ = conn.read(&mut buf);
                    total += 1;
                }
                total
            })
        })
        .collect();

    let totals: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let total: u64 = totals.iter().sum();
    let elapsed = start.elapsed().as_secs_f64();
    let qps = total as f64 / elapsed;

    println!("TCP ping-pong: {total} echoes in {elapsed:.2}s");
    println!("QPS: {qps:.0}");
    println!("Connections: {num_conns}");
    println!("Threads: {num_conns}");
    println!("Message size: {} bytes", msg.len());
}
