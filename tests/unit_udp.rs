use rust_miniss::net::AsyncUdpSocket;
use rust_miniss::Runtime;
use std::net::SocketAddr;

#[test]
fn test_udp_bind() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let socket = AsyncUdpSocket::bind(addr).unwrap();
    assert!(socket.local_addr().is_ok());
}

#[test]
fn test_udp_vectored_send() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let sender: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let receiver: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let sender_socket = AsyncUdpSocket::bind(sender).unwrap();
        let receiver_socket = AsyncUdpSocket::bind(receiver).unwrap();
        let receiver_addr = receiver_socket.local_addr().unwrap();

        let bufs: &[&[u8]] = &[b"hello", b"world"];
        let total_len: usize = bufs.iter().map(|b| b.len()).sum();
        assert_eq!(total_len, 10);

        for buf in bufs {
            let sent = sender_socket.send_to(buf, receiver_addr).await.unwrap();
            assert_eq!(sent, buf.len());
        }

        let mut recv_buf = [0u8; 20];
        let (n, _) = receiver_socket.recv_from(&mut recv_buf).await.unwrap();
        assert_eq!(&recv_buf[..n], b"hello");

        let (n, _) = receiver_socket.recv_from(&mut recv_buf).await.unwrap();
        assert_eq!(&recv_buf[..n], b"world");
    });
}
