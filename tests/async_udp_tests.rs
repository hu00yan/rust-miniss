//! Tests for async UDP functionality.

use rust_miniss::{net::AsyncUdpSocket, Runtime};
use std::net::SocketAddr;

#[test]
fn test_async_udp_send_recv() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Bind to local addresses
        let addr1: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket1 = AsyncUdpSocket::bind(addr1).expect("Failed to bind socket1");
        let local_addr1 = socket1.local_addr().expect("Failed to get local address 1");

        let addr2: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket2 = AsyncUdpSocket::bind(addr2).expect("Failed to bind socket2");
        let local_addr2 = socket2.local_addr().expect("Failed to get local address 2");

        // Send data from socket1 to socket2
        let test_data = b"Hello, async UDP!";
        let bytes_sent = socket1
            .send_to(test_data, local_addr2)
            .await
            .expect("Failed to send");
        assert_eq!(bytes_sent, test_data.len());

        // Receive data on socket2
        let mut buf = [0; 1024];
        let (bytes_received, src_addr) = socket2
            .recv_from(&mut buf)
            .await
            .expect("Failed to receive");
        assert_eq!(bytes_received, test_data.len());
        assert_eq!(&buf[..bytes_received], test_data);
        assert_eq!(src_addr, local_addr1);
    });
}

#[test]
fn test_async_udp_multiple_sends() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Bind to local addresses
        let addr1: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket1 = AsyncUdpSocket::bind(addr1).expect("Failed to bind socket1");
        let local_addr1 = socket1.local_addr().expect("Failed to get local address 1");

        let addr2: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket2 = AsyncUdpSocket::bind(addr2).expect("Failed to bind socket2");
        let local_addr2 = socket2.local_addr().expect("Failed to get local address 2");

        // Send multiple messages
        let messages = vec![
            b"Message 1".to_vec(),
            b"Message 2".to_vec(),
            b"Message 3".to_vec(),
        ];

        for msg in &messages {
            let bytes_sent = socket1
                .send_to(msg, local_addr2)
                .await
                .expect("Failed to send");
            assert_eq!(bytes_sent, msg.len());
        }

        // Receive all messages
        for msg in &messages {
            let mut buf = [0; 1024];
            let (bytes_received, src_addr) = socket2
                .recv_from(&mut buf)
                .await
                .expect("Failed to receive");
            assert_eq!(bytes_received, msg.len());
            assert_eq!(&buf[..bytes_received], msg.as_slice());
            assert_eq!(src_addr, local_addr1);
        }
    });
}
