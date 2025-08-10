use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::thread;

// Minimal HTTP/1.1 request parser: read until \r\n\r\n, parse request line
fn read_http_request(stream: &mut TcpStream) -> std::io::Result<(String, String, String)> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut buf = [0u8; 4096];
    let mut data = Vec::with_capacity(4096);
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if data.len() > 16 * 1024 {
            break; // avoid unbounded growth
        }
    }
    let text = String::from_utf8_lossy(&data);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("").to_string();
    Ok((method, path, version))
}

fn write_http_response(mut stream: TcpStream) -> std::io::Result<()> {
    // Always respond 200 OK with fixed body
    let body = b"Hello from rust-miniss";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn handle_client(mut stream: TcpStream) {
    // Parse minimal request; ignore errors and still write response
    let _ = read_http_request(&mut stream);
    let _ = write_http_response(stream);
}

fn run_single_thread(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    println!("http_echo listening on {} (single-thread)", addr);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => handle_client(stream),
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
    Ok(())
}

fn worker(rx: Receiver<TcpStream>) {
    for stream in rx {
        handle_client(stream);
    }
}

fn run_multicore(addr: &str, workers: usize) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;

    let (tx, rx): (Sender<TcpStream>, Receiver<TcpStream>) = unbounded();

    // Spawn worker threads
    let mut threads = Vec::with_capacity(workers);
    for _ in 0..workers {
        let rx_clone = rx.clone();
        threads.push(thread::spawn(move || worker(rx_clone)));
    }

    println!(
        "http_echo listening on {} (multi-core with {} workers)",
        addr, workers
    );

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if tx.send(stream).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // brief sleep to avoid busy loop
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => {
                eprintln!("accept error: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    drop(tx);
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

fn parse_args() -> (String, bool, usize) {
    // Returns (addr, multicore, workers)
    let mut addr = "0.0.0.0:8080".to_string();
    let mut multicore = false;
    let mut workers = std::cmp::max(1, num_cpus::get());

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => {
                if let Some(v) = args.next() {
                    addr = v;
                }
            }
            "--multi" | "-m" => {
                multicore = true;
            }
            "--workers" | "-w" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        workers = std::cmp::max(1, n);
                    }
                }
            }
            _ => {}
        }
    }

    (addr, multicore, workers)
}

fn main() -> std::io::Result<()> {
    let (addr, multicore, workers) = parse_args();
    if multicore {
        run_multicore(&addr, workers)
    } else {
        run_single_thread(&addr)
    }
}
