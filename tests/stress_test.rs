use rust_miniss::net::AsyncUdpSocket;
use rust_miniss::timer;
use rust_miniss::Runtime;
use std::time::Duration;

#[test]
fn test_udp_stress() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let recv_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();

        let send_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();

        let count = 1000;
        for i in 0..count {
            let data = format!("packet {i}").into_bytes();
            send_socket.send_to(&data, recv_addr).await.unwrap();
        }

        for i in 0..count {
            let mut buf = [0u8; 64];
            let (n, _from) = recv_socket.recv_from(&mut buf).await.unwrap();
            let expected = format!("packet {i}");
            assert_eq!(&buf[..n], expected.as_bytes());
        }
    });
}

#[test]
fn test_long_running() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        for _ in 0..10 {
            timer::sleep(Duration::from_millis(10)).await;
        }
    });
}

#[test]
fn test_udp_stress_multiple_senders() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let recv_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();

        let sender_a =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let sender_b =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();

        let n = 500;
        for i in 0..n {
            let data = format!("A{i}").into_bytes();
            sender_a.send_to(&data, recv_addr).await.unwrap();
            let data = format!("B{i}").into_bytes();
            sender_b.send_to(&data, recv_addr).await.unwrap();
        }

        let mut count_a = 0;
        let mut count_b = 0;
        for _ in 0..n * 2 {
            let mut buf = [0u8; 64];
            let (_len, _) = recv_socket.recv_from(&mut buf).await.unwrap();
            if buf[0] == b'A' {
                count_a += 1;
            } else if buf[0] == b'B' {
                count_b += 1;
            }
        }
        assert_eq!(count_a, n);
        assert_eq!(count_b, n);
    });
}
