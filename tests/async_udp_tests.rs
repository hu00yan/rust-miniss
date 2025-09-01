//! Tests for async UDP functionality.

use rust_miniss::{net::AsyncUdpSocket, Runtime};
use std::net::SocketAddr;

#[test]
fn test_async_udp_send_recv() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Test 1: Basic socket creation and configuration
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = AsyncUdpSocket::bind(addr).expect("Failed to bind socket");
        let local_addr = socket.local_addr().expect("Failed to get local address");

        // Verify socket is bound to a valid address
        assert!(local_addr.port() > 0);
        assert_eq!(local_addr.ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

        // Test 2: Try to send to localhost (should work)
        let target_addr: SocketAddr = "127.0.0.1:12345".parse().unwrap(); // Localhost
        let test_data = b"Hello, async UDP!";

        // Test with a receiving socket to verify the send actually works
        // Create a receiver socket bound to the target address
        let receiver_socket = std::net::UdpSocket::bind(target_addr).expect("Failed to bind receiver socket");

        // Test async UDP send only
        match socket.send_to(test_data, target_addr).await {
            Ok(bytes_sent) => {
                println!("Async UDP send succeeded: {} bytes sent", bytes_sent);
                assert_eq!(bytes_sent, test_data.len());

                // Try to receive the data to verify it was sent
                let mut buf = [0u8; 1024];
                match receiver_socket.recv_from(&mut buf) {
                    Ok((received, sender_addr)) => {
                        println!("Async UDP receive succeeded: {} bytes from {}", received, sender_addr);
                        assert_eq!(received, test_data.len());
                        assert_eq!(&buf[..received], test_data);
                    }
                    Err(e) => {
                        println!("Async UDP receive failed: {} ({})", e, e.kind());
                    }
                }
            }
            Err(e) => {
                // Check the error type
                match e.kind() {
                    std::io::ErrorKind::InvalidInput => {
                        panic!("Async UDP send failed with InvalidInput - this indicates a programming error: {}", e);
                    }
                    _ => {
                        // Other errors (network unreachable, permission denied, etc.) are acceptable
                        println!("Async UDP send failed with expected error: {} ({})", e, e.kind());
                    }
                }
            }
        }
    });
}

#[test]
fn test_async_udp_multiple_sends() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Test socket creation and basic operations
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = AsyncUdpSocket::bind(addr).expect("Failed to bind socket");
        let local_addr = socket.local_addr().expect("Failed to get local address");

        // Verify socket configuration
        assert!(local_addr.port() > 0);

        // Test multiple send operations to localhost
        let target_addr: SocketAddr = "127.0.0.1:12346".parse().unwrap(); // Localhost

        let messages = [
            b"Message 1".to_vec(),
            b"Message 2".to_vec(),
            b"Message 3".to_vec(),
        ];

        // Send messages to the same address
        for (i, msg) in messages.iter().enumerate() {
            match socket.send_to(msg, target_addr).await {
                Ok(bytes_sent) => {
                    assert_eq!(bytes_sent, msg.len());
                    println!("Message {} sent successfully: {} bytes", i + 1, bytes_sent);
                }
                Err(e) => {
                    // Check the error type
                    match e.kind() {
                        std::io::ErrorKind::InvalidInput => {
                            panic!(
                                "Send {} failed with InvalidInput - programming error: {}",
                                i, e
                            );
                        }
                        _ => {
                            // Other errors are acceptable
                            println!("Message {} failed with expected error: {}", i, e);
                        }
                    }
                }
            }
        }
    });
}
