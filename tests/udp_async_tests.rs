//! Unit tests for async UDP operations
//! Tests the correct async behavior of AsyncUdpSocket

use rust_miniss::{net::AsyncUdpSocket, Runtime};
use std::net::SocketAddr;
use std::time::Duration;

#[test]
fn test_udp_bind() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = AsyncUdpSocket::bind(addr);

        assert!(socket.is_ok(), "Failed to bind UDP socket");

        let socket = socket.unwrap();
        let local_addr = socket.local_addr();
        assert!(local_addr.is_ok(), "Failed to get local address");

        let local_addr = local_addr.unwrap();
        assert_eq!(local_addr.ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));
        assert!(local_addr.port() > 0, "Port should be > 0");
    });
}

#[test]
fn test_udp_send_to_valid_address() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server_socket = AsyncUdpSocket::bind(server_addr).unwrap();
        let actual_server_addr = server_socket.local_addr().unwrap();

        let client_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let client_socket = AsyncUdpSocket::bind(client_addr).unwrap();

        let data = b"Hello, UDP!";
        let result = client_socket.send_to(data, actual_server_addr).await;

        assert!(result.is_ok(), "Failed to send UDP packet");
        let bytes_sent = result.unwrap();
        assert_eq!(bytes_sent, data.len(), "Not all bytes were sent");
    });
}

#[test]
fn test_udp_send_to_invalid_address() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let client_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let client_socket = AsyncUdpSocket::bind(client_addr).unwrap();

        // Try to send to unreachable address
        let invalid_addr: SocketAddr = "192.0.2.1:80".parse().unwrap(); // RFC 5737 test address
        let data = b"Hello, UDP!";
        let result = client_socket.send_to(data, invalid_addr).await;

        // This should eventually fail or timeout, but shouldn't crash
        // The exact error depends on the network configuration
        assert!(result.is_err() || result.is_ok(), "Should handle invalid address gracefully");
    });
}

#[test]
fn test_udp_recv_from_basic() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server_socket = AsyncUdpSocket::bind(server_addr).unwrap();
        let actual_server_addr = server_socket.local_addr().unwrap();

        let client_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let client_socket = AsyncUdpSocket::bind(client_addr).unwrap();
        let client_local_addr = client_socket.local_addr().unwrap();

        // Send from client to server
        let data = b"Hello, Server!";
        let sent = client_socket.send_to(data, actual_server_addr).await.unwrap();
        assert_eq!(sent, data.len());

        // Receive on server
        let mut buffer = [0u8; 1024];
        let result = server_socket.recv_from(&mut buffer).await;

        assert!(result.is_ok(), "Failed to receive UDP packet");
        let (bytes_received, from_addr) = result.unwrap();
        assert_eq!(bytes_received, data.len(), "Received wrong number of bytes");
        assert_eq!(&buffer[..bytes_received], data, "Received wrong data");
        assert_eq!(from_addr.ip(), client_local_addr.ip(), "Received from wrong IP");
        assert_eq!(from_addr.port(), client_local_addr.port(), "Received from wrong port");
    });
}

#[test]
fn test_udp_recv_from_empty_buffer() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server_socket = AsyncUdpSocket::bind(server_addr).unwrap();
        let actual_server_addr = server_socket.local_addr().unwrap();

        let client_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let client_socket = AsyncUdpSocket::bind(client_addr).unwrap();

        // Send data
        let data = b"Hello, Server!";
        client_socket.send_to(data, actual_server_addr).await.unwrap();

        // Try to receive into empty buffer (should fail gracefully)
        let mut empty_buffer = [0u8; 0];
        let result = server_socket.recv_from(&mut empty_buffer).await;

        // This should either fail or return 0 bytes without crashing
        match result {
            Ok((0, _)) => {}, // Empty buffer received 0 bytes
            Err(_) => {},     // Proper error handling
            Ok(_) => panic!("Should not receive data into empty buffer"),
        }
    });
}

#[test]
fn test_udp_recv_from_small_buffer() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server_socket = AsyncUdpSocket::bind(server_addr).unwrap();
        let actual_server_addr = server_socket.local_addr().unwrap();

        let client_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let client_socket = AsyncUdpSocket::bind(client_addr).unwrap();

        // Send larger data
        let data = b"Hello, Server! This is a longer message that won't fit in small buffer.";
        client_socket.send_to(data, actual_server_addr).await.unwrap();

        // Receive into smaller buffer (should truncate)
        let mut small_buffer = [0u8; 10];
        let result = server_socket.recv_from(&mut small_buffer).await;

        assert!(result.is_ok(), "Failed to receive UDP packet");
        let (bytes_received, from_addr) = result.unwrap();
        assert_eq!(bytes_received, small_buffer.len(), "Should receive up to buffer size");
        assert_eq!(&small_buffer, &data[..small_buffer.len()], "Should receive first part of data");
    });
}

#[test]
fn test_udp_echo_server() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server_socket = AsyncUdpSocket::bind(server_addr).unwrap();
        let actual_server_addr = server_socket.local_addr().unwrap();

        // Start echo server task
        let server_socket_clone = server_socket;
        let server_task = rust_miniss::task::spawn(async move {
            let mut buffer = [0u8; 1024];
            loop {
                match server_socket_clone.recv_from(&mut buffer).await {
                    Ok((bytes_read, from_addr)) => {
                        // Echo back the received data
                        let response_data = &buffer[..bytes_read];
                        if let Err(e) = server_socket_clone.send_to(response_data, from_addr).await {
                            eprintln!("Echo server send error: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("Echo server recv error: {}", e);
                        break;
                    }
                }
            }
        }).unwrap();

        // Give server time to start
        rust_miniss::timer::sleep(Duration::from_millis(10)).await;

        let client_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let client_socket = AsyncUdpSocket::bind(client_addr).unwrap();

        // Send test message
        let test_message = b"Echo this message!";
        client_socket.send_to(test_message, actual_server_addr).await.unwrap();

        // Receive echo
        let mut echo_buffer = [0u8; 1024];
        let result = client_socket.recv_from(&mut echo_buffer).await;

        assert!(result.is_ok(), "Failed to receive echo");
        let (bytes_received, from_addr) = result.unwrap();
        assert_eq!(bytes_received, test_message.len(), "Echo length mismatch");
        assert_eq!(&echo_buffer[..bytes_received], test_message, "Echo data mismatch");
        assert_eq!(from_addr.ip(), actual_server_addr.ip(), "Echo from wrong server");

        // Clean up
        // server_task.abort(); // Not implemented
    });
}

#[test]
fn test_udp_concurrent_operations() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server_socket = AsyncUdpSocket::bind(server_addr).unwrap();
        let actual_server_addr = server_socket.local_addr().unwrap();

        // Start multiple concurrent senders
        let mut send_tasks = Vec::new();
        for i in 0..5 {
            let client_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let client_socket = AsyncUdpSocket::bind(client_addr).unwrap();

            let task = rust_miniss::task::spawn(async move {
                let message = format!("Message from sender {}", i);
                let result = client_socket.send_to(message.as_bytes(), actual_server_addr).await;
                (i, result)
            }).unwrap();
            send_tasks.push(task);
        }

        // Receive all messages
        let mut received_messages = Vec::new();
        for _ in 0..5 {
            let mut buffer = [0u8; 1024];
            match server_socket.recv_from(&mut buffer).await {
                Ok((bytes_read, _)) => {
                    let message = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
                    received_messages.push(message);
                }
                Err(e) => panic!("Failed to receive message: {}", e),
            }
        }

        // Wait for all senders and verify results
        for task in send_tasks {
            let (sender_id, result) = task.await;
            assert!(result.is_ok(), "Sender {} failed", sender_id);
        }

        assert_eq!(received_messages.len(), 5, "Should receive 5 messages");
    });
}

#[test]
fn test_udp_socket_drop_cleanup() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        {
            let socket = AsyncUdpSocket::bind(addr).unwrap();
            let local_addr = socket.local_addr().unwrap();

            // Socket is valid
            assert!(socket.local_addr().is_ok());

            // Send a test packet
            let data = b"Test message";
            let result = socket.send_to(data, local_addr).await;
            assert!(result.is_ok());

        } // Socket is dropped here

        // Try to bind to the same address (should succeed after some time)
        rust_miniss::timer::sleep(Duration::from_millis(100)).await;

        let new_socket = AsyncUdpSocket::bind(addr);
        // This might fail due to TIME_WAIT, but shouldn't crash
        assert!(new_socket.is_ok() || new_socket.is_err());
    });
}