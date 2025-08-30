//! Comprehensive async UDP demo showing various UDP operations.

use rust_miniss::{net::AsyncUdpSocket, Runtime};
use std::net::SocketAddr;

fn main() -> std::io::Result<()> {
    let runtime = Runtime::new();

    runtime.block_on(async move {
        println!("=== Async UDP Demo ===");
        println!("This demo shows various UDP operations:");
        println!("1. Basic send/receive to self");
        println!("2. Multiple send operations");
        println!("3. Error handling");
        println!();

        // Demo 1: Basic send/receive
        println!("1. Basic UDP send/receive:");
        let addr: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        let socket = match AsyncUdpSocket::bind(addr) {
            Ok(sock) => sock,
            Err(e) => {
                eprintln!("   Failed to bind socket: {}", e);
                return;
            }
        };

        match socket.local_addr() {
            Ok(local_addr) => println!("   Bound to local address: {}", local_addr),
            Err(e) => {
                eprintln!("   Failed to get local address: {}", e);
                return;
            }
        }

        let test_data = b"Hello, async UDP!";
        let bytes_sent = match socket.send_to(test_data, addr).await {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("   Failed to send data: {}", e);
                return;
            }
        };
        println!(
            "   Sent {} bytes: {:?}",
            bytes_sent,
            String::from_utf8_lossy(test_data)
        );

        // Receive the data back
        let mut buf = [0u8; 1024];
        let (bytes_read, sender_addr) = match socket.recv_from(&mut buf).await {
            Ok(result) => result,
            Err(e) => {
                eprintln!("   Failed to receive data: {}", e);
                return;
            }
        };
        println!(
            "   Received {} bytes from {}: {:?}",
            bytes_read,
            sender_addr,
            String::from_utf8_lossy(&buf[..bytes_read])
        );
        println!();

        // Demo 2: Multiple sends
        println!("2. Multiple UDP sends:");
        for i in 1..=3 {
            let message = format!("Message {}", i);
            let data = message.as_bytes();
            let bytes_sent = match socket.send_to(data, addr).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("   Failed to send message {}: {}", i, e);
                    continue;
                }
            };
            println!("   Sent message {}: {} bytes", i, bytes_sent);

            // Receive response
            let (bytes_read, _) = match socket.recv_from(&mut buf).await {
                Ok(result) => result,
                Err(e) => {
                    eprintln!("   Failed to receive response for message {}: {}", i, e);
                    continue;
                }
            };
            println!(
                "   Received response: {:?}",
                String::from_utf8_lossy(&buf[..bytes_read])
            );
        }
        println!();

        // Demo 3: Error handling
        println!("3. Error handling demo:");
        let invalid_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        match socket.send_to(b"Test error", invalid_addr).await {
            Ok(bytes) => println!("   Unexpected success: sent {} bytes", bytes),
            Err(e) => println!("   Expected error: {}", e),
        }

        println!();
        println!("UDP demo completed successfully!");
    });

    Ok(())
}
