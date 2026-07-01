//! Tests for async UDP functionality.

use rust_miniss::{net::AsyncUdpSocket, Buffer, Runtime};
use std::net::SocketAddr;

#[test]
fn test_async_udp_send_recv() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = AsyncUdpSocket::bind(addr).expect("Failed to bind socket");
        let local_addr = socket.local_addr().expect("Failed to get local address");

        assert!(local_addr.port() > 0);
        assert_eq!(
            local_addr.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );

        let receiver_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let receiver_socket =
            AsyncUdpSocket::bind(receiver_addr).expect("Failed to bind receiver socket");
        let receiver_addr = receiver_socket.local_addr().unwrap();

        let test_data = b"Hello, async UDP!";

        let bytes_sent = socket
            .send_to(test_data, receiver_addr)
            .await
            .expect("Async UDP send failed");
        assert_eq!(bytes_sent, test_data.len());

        let buf = Buffer::with_capacity(1024);
        let (bytes_received, _sender, buf) = receiver_socket
            .recv_from_rent(buf)
            .await
            .expect("Async UDP receive failed");
        assert_eq!(bytes_received, test_data.len());
        assert_eq!(&buf[..bytes_received], test_data);
    });
}

#[test]
fn test_async_udp_multiple_sends() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = AsyncUdpSocket::bind(addr).expect("Failed to bind socket");

        let receiver_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let receiver_socket =
            AsyncUdpSocket::bind(receiver_addr).expect("Failed to bind receiver socket");
        let receiver_addr = receiver_socket.local_addr().unwrap();

        let messages: &[&[u8]] = &[b"Message 1", b"Message 2", b"Message 3"];

        for msg in messages {
            let bytes_sent = socket
                .send_to(msg, receiver_addr)
                .await
                .expect("Send failed");
            assert_eq!(bytes_sent, msg.len());
        }

        let mut buf = [0u8; 1024];
        for msg in messages {
            let (bytes_received, _sender) = receiver_socket
                .recv_from(&mut buf)
                .await
                .expect("Receive failed");
            assert_eq!(&buf[..bytes_received], *msg);
        }
    });
}
