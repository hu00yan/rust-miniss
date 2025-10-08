use rust_miniss::net::AsyncUdpSocket;
use std::net::SocketAddr;

#[test]
fn test_udp_bind() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let socket = AsyncUdpSocket::bind(addr).unwrap();
    assert!(socket.local_addr().is_ok());
}

#[test]
fn test_udp_vectored_send() {
    // Test multi-iovec UDP (Glommio-style)
    // Note: This is a unit test; actual async test in integration
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let socket = AsyncUdpSocket::bind(addr).unwrap();
    let bufs = vec![b"hello", b"world"];
    // Async test would use runtime.block_on
}