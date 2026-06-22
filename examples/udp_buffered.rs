//! UDP buffered echo server example.
//!
//! Run with:
//! ```
//! cargo run --example udp_buffered
//! ```
//!
//! Test with:
//! ```
//! echo "Hello UDP" | nc -u 127.0.0.1 8081
//! ```

use rust_miniss::net::AsyncUdpSocket;
use rust_miniss::MultiCoreRuntime;
use std::net::SocketAddr;

fn main() {
    let addr: SocketAddr = "127.0.0.1:8081".parse().unwrap();
    println!("Starting UDP buffered server on {addr}");
    println!("Test with: echo \"Hello UDP\" | nc -u {addr}");

    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();

    runtime.block_on(async {
        let socket = AsyncUdpSocket::bind(addr).unwrap();
        println!("Listening on {addr}");

        let mut buf = vec![0u8; 1024];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, peer)) => {
                    let msg = String::from_utf8_lossy(&buf[..len]);
                    println!("Received {len} bytes from {peer}: {msg}");

                    // Echo back
                    if let Err(e) = socket.send_to(&buf[..len], peer).await {
                        eprintln!("Failed to send response: {e}");
                    } else {
                        println!("Echoed {len} bytes to {peer}");
                    }
                }
                Err(e) => {
                    eprintln!("Receive error: {e}");
                    break;
                }
            }
        }
    });

    runtime.shutdown().unwrap();
}
