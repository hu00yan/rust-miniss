use futures::future::{join, join_all};
use rust_miniss::{net::AsyncUdpSocket, timer, Runtime};
use std::net::SocketAddr;
use std::time::Duration;

#[test]
fn test_udp_send_recv_basic() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let server_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let server_addr = server_socket.local_addr().unwrap();

        let client_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let client_addr = client_socket.local_addr().unwrap();

        let server = async move {
            let mut buffer = [0u8; 32];
            let (bytes, from) = server_socket.recv_from(&mut buffer).await.unwrap();
            assert_eq!(bytes, b"Hello, UDP".len());
            assert_eq!(&buffer[..bytes], b"Hello, UDP");
            assert_eq!(from, client_addr);
        };

        let client = async move {
            client_socket
                .send_to(b"Hello, UDP", server_addr)
                .await
                .unwrap();
        };

        join(server, client).await;
    });
}

#[test]
fn test_udp_truncated_receive() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let server_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let server_addr = server_socket.local_addr().unwrap();

        let client_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();

        let server = async move {
            let mut buffer = [0u8; 8];
            let (bytes, _) = server_socket.recv_from(&mut buffer).await.unwrap();
            assert_eq!(bytes, buffer.len());
            assert_eq!(&buffer, b"Truncate");
        };

        let client = async move {
            client_socket
                .send_to(b"Truncated payload", server_addr)
                .await
                .unwrap();
        };

        join(server, client).await;
    });
}

#[test]
fn test_udp_echo_roundtrip() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let server_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let server_addr = server_socket.local_addr().unwrap();

        let server = async move {
            let mut buffer = [0u8; 128];
            loop {
                match server_socket.recv_from(&mut buffer).await {
                    Ok((0, _)) => break,
                    Ok((n, from)) => {
                        if &buffer[..n] == b"STOP" {
                            break;
                        }
                        server_socket.send_to(&buffer[..n], from).await.unwrap();
                    }
                    Err(_) => break,
                }
            }
        };

        let client = async move {
            timer::sleep(Duration::from_millis(10)).await;
            let socket =
                AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
            socket.send_to(b"Echo this", server_addr).await.unwrap();

            let mut buf = [0u8; 32];
            let (bytes, from) = socket.recv_from(&mut buf).await.unwrap();
            assert_eq!(from, server_addr);
            assert_eq!(&buf[..bytes], b"Echo this");

            socket.send_to(b"STOP", server_addr).await.unwrap();
        };

        join(server, client).await;
    });
}

#[test]
fn test_udp_multiple_senders() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let server_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let server_addr = server_socket.local_addr().unwrap();

        let server = async move {
            let mut received = Vec::new();
            let mut buffer = [0u8; 64];
            for _ in 0..5 {
                let (bytes, _) = server_socket.recv_from(&mut buffer).await.unwrap();
                received.push(String::from_utf8_lossy(&buffer[..bytes]).to_string());
            }
            received
        };

        let clients = (0..5).map(|i| {
            let addr = server_addr;
            async move {
                let socket =
                    AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
                let message = format!("message-{i}");
                socket.send_to(message.as_bytes(), addr).await.unwrap();
            }
        });

        let (received, _) = join(server, join_all(clients)).await;
        assert_eq!(received.len(), 5);
    });
}
