#[cfg(test)]
mod vectored_tests {
    use super::super::net::AsyncTcpStream;
    use std::io::{IoSlice, IoSliceMut};
    use std::net::TcpListener;

    #[tokio::test]
    async fn test_vectored_tcp_read() {
        let rt = Runtime::new();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let mut server = listener.accept().unwrap().0;
            server.set_nonblocking(true).unwrap();
            let mut server_stream = AsyncTcpStream::from(server);

            let client = std::net::TcpStream::connect(addr).unwrap();
            client.write_all(b"multi part data").unwrap();

            let mut bufs = vec![IoSliceMut::new(&mut [0u8; 5]), IoSliceMut::new(&mut [0u8; 9])];
            let n = server_stream.read_vectored(&mut bufs).await.unwrap();
            assert_eq!(n, 14);
            assert_eq!(&bufs[0][..5], b"multi");
            assert_eq!(&bufs[1][..9], b" part data");

            // Partial (simulate)
            let partial_bufs = vec![IoSliceMut::new(&mut [0u8; 3])];
            let partial_n = server_stream.read_vectored(&mut partial_bufs).await.unwrap();
            assert_eq!(partial_n, 0);  // No more
        });
    }

    #[tokio::test]
    async fn test_vectored_udp() {
        let rt = Runtime::new();
        rt.block_on(async {
            let socket = AsyncUdpSocket::bind("127.0.0.1:0").unwrap();
            let peer = "127.0.0.1:9090".parse().unwrap();
            let slices = vec![IoSlice::new(b"batched"), IoSlice::new(b" datagram")];
            let n = socket.send_to_vectored(&slices, peer).await.unwrap();  // Assume added method
            assert_eq!(n, 16);
        });
    }
}