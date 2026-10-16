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

#[test]
fn test_udp_pending_receive_is_cancelled_by_timeout() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let socket = AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let mut buffer = [0u8; 32];
        let result = timer::timeout(Duration::from_millis(20), socket.recv_from(&mut buffer)).await;
        assert!(
            result.is_err(),
            "pending UDP receive unexpectedly completed"
        );
    });
}

#[test]
fn test_udp_receive_can_be_reused_after_cancellation() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let receiver = AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let sender = AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let receiver_addr = receiver.local_addr().unwrap();
        let mut buffer = [0u8; 32];

        let cancelled =
            timer::timeout(Duration::from_millis(20), receiver.recv_from(&mut buffer)).await;
        assert!(
            cancelled.is_err(),
            "receive unexpectedly completed before send"
        );

        sender
            .send_to(b"after-cancel", receiver_addr)
            .await
            .unwrap();
        let (bytes, _) = timer::timeout(Duration::from_secs(1), receiver.recv_from(&mut buffer))
            .await
            .expect("receive after cancellation timed out")
            .unwrap();
        assert_eq!(&buffer[..bytes], b"after-cancel");
    });
}

/// The UDP send-side rent contract: the buffer comes back, and it is the
/// *same* allocation rather than a copy of it.
///
/// The pointer assertion is the point of this test. `Buffer::clone` deep-copies
/// the payload (`src/buffer.rs:229-235`), and the mio backend used to clone the
/// datagram on completion, so a `send_to_rent` that copied would fail here on
/// macOS/kqueue while passing on io_uring — which is exactly the asymmetry
/// between the two backends this covers.
#[test]
fn test_udp_send_to_rent_returns_the_same_allocation() {
    use rust_miniss::Buffer;

    let runtime = Runtime::new();
    runtime.block_on(async {
        let receiver = AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let receiver_addr = receiver.local_addr().unwrap();
        let sender = AsyncUdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();

        let payload: &[u8] = b"rented datagram";
        let owned = Buffer::from_slice(payload);
        let original_ptr = owned.as_ref().as_ptr();

        let receiving = async move {
            let mut buffer = [0u8; 64];
            for _ in 0..2 {
                let (bytes, _) = receiver
                    .recv_from(&mut buffer)
                    .await
                    .expect("receive rented datagram");
                assert_eq!(&buffer[..bytes], payload);
            }
        };

        let sending = async move {
            let (sent, returned) = sender
                .send_to_rent(owned, receiver_addr)
                .await
                .expect("send rented datagram");
            assert_eq!(sent, payload.len());
            assert_eq!(
                returned.as_ref().as_ptr(),
                original_ptr,
                "send_to_rent must return the same allocation, not a copy"
            );
            // Rent semantics: what comes back is immediately reusable.
            let (resent, _) = sender
                .send_to_rent(returned, receiver_addr)
                .await
                .expect("reuse rented datagram");
            assert_eq!(resent, payload.len());
        };

        join(receiving, sending).await;
    });
}
