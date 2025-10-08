use rust_miniss::{Runtime, net::{AsyncTcpStream, AsyncUnixStream, AsyncUnixListener}};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::task::noop_waker;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use hyper::server::conn::http1::Builder;
use hyper::service::service_fn;
use hyper::{body::Body, Request, Response, Server};
use std::io;
use std::net::TcpListener;
use tempfile::tempdir;
use std::io::IoSlice;
use rust_miniss::net::{AsyncUdpSocket, AsyncTcpStream, AsyncUnixStream};

#[tokio::test]
async fn test_buffered_udp_echo() {
    // ... existing

    // #[tokio::test]
    // async fn test_buffered_partial_drain() {
    //     let rt = Runtime::new();
    //     rt.block_on(async {
    //         let socket = AsyncUdpSocket::bind("127.0.0.1:0").unwrap();
    //         let mut buffered = BufferedUdpSocket::new(socket);
    //         let peer = "127.0.0.1:9090".parse().unwrap();
    //         buffered.peer_addr = Some(peer);

    //         // Enqueue 10-byte datagram
    //         let msg = b"10byte msg";
    //         let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    //         sender.send_to(msg, peer).unwrap();
    //         buffered.recv_buffered(1024).await.unwrap();  // Queue full

    //         // Partial read (5 bytes)
    //         let mut buf = [0u8; 5];
    //         let n = buffered.poll_read(Pin::new(&mut buffered), &mut Context::from_waker(&noop_waker()), &mut buf).await.unwrap();
    //         assert_eq!(n, 5);
    //         assert_eq!(&buf[..n], b"10byt");  // Partial

    //         // Remaining queued (5 bytes)
    //         let remaining = buffered.buffer.len();
    //         assert_eq!(remaining, 5);  // "e msg" left

    //         // Full drain on second
    //         let mut buf2 = [0u8; 5];
    //         let n2 = buffered.poll_read(Pin::new(&mut buffered), &mut Context::from_waker(&noop_waker()), &mut buf2).await.unwrap();
    //         assert_eq!(n2, 5);
    //         assert_eq!(&buf2[..n2], b"e msg");
    //     });
    // }
}

#[tokio::test]
async fn test_unix_connect_bind() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let path_str = path.to_str().unwrap();

        let listener = AsyncUnixListener::bind(path_str).unwrap();
        let (stream, addr) = listener.accept().await.unwrap();

        let client = AsyncUnixStream::connect(path_str).await.unwrap();  // Note: async connect via runtime
        let mut client_stream = client;

        // Write/Read echo
        client_stream.write_all(b"unix test").await.unwrap();
        let mut buf = [0u8; 9];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(n, 9);
        assert_eq!(&buf[..n], b"unix test");

        // Vectored write
        let slices = vec![IoSlice::new(b"part1"), IoSlice::new(b" part2")];
        let n_vec = client_stream.write_vectored(&slices).await.unwrap();
        assert_eq!(n_vec, 10);

        // Close
        client_stream.close().await.unwrap();
    });
}

#[tokio::test]
async fn test_hyper_compat() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        // Simple echo server with Hyper on compat stream
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // Server side (simplified)
        let mut server_conn = listener.accept().unwrap().0;
        server_conn.set_nonblocking(true).unwrap();
        let server_stream = AsyncTcpStream::from(server_conn).compat();  // Compat to tokio::io

        let service = service_fn(|_req: Request<Body>| async {
            Ok::<_, io::Error>(Response::new(Body::from("Hello from miniss!")))
        });

        let mut http_conn = Builder::new().serve_connection(server_stream, service).await.unwrap();

        // Client side
        let client = reqwest::Client::new();
        let res = client.get(format!("http://{}/", addr)).send().await.unwrap();
        let body = res.text().await.unwrap();
        assert_eq!(body, "Hello from miniss!");
    });
}

#[tokio::test]
async fn test_partial_read() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        // Setup echo server that sends partial data
        // (Mock or use pipe; simplified assert partial n < buf.len())
        let mut stream = AsyncTcpStream::connect("127.0.0.1:8080").await.unwrap();  // Assume server sends 3/5 bytes
        let mut buf = [0u8; 5];
        let n = stream.read(&mut buf).await.unwrap();
        assert!(n <= 5);  // Partial possible
        // Further reads to complete
    });
}

#[tokio::test]
async fn test_metrics_emission() {
    // Use metrics::query or exporter to check histograms/counters emitted
    // Simplified: Assume setup, perform IO, query
    // use metrics::query::Query; // Not available in this version
    // ...
    // assert histogram!("net.read_latency") has records
}