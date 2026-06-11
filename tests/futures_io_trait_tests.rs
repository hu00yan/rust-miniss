use futures::future;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Runtime;
use std::net::SocketAddr;

#[test]
fn async_read_write_roundtrip() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 16];
            let n = stream.read(&mut buffer).await.unwrap();
            assert_eq!(&buffer[..n], b"ping");
            stream.write_all(b"pong").await.unwrap();
            stream.flush().await.unwrap();
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"ping").await.unwrap();
            stream.flush().await.unwrap();

            let mut response = [0u8; 16];
            let n = stream.read(&mut response).await.unwrap();
            assert_eq!(&response[..n], b"pong");
        };

        future::join(server, client).await;
    });
}

#[test]
fn async_partial_reads() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut first = [0u8; 5];
            stream.read_exact(&mut first).await.unwrap();
            assert_eq!(&first, b"Hello");

            let mut rest = [0u8; 7];
            stream.read_exact(&mut rest).await.unwrap();
            assert_eq!(&rest, b", world");
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"Hello, world").await.unwrap();
            stream.flush().await.unwrap();
        };

        future::join(server, client).await;
    });
}

#[test]
fn async_shutdown_notifies_peer() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8];
            let n = stream.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);
        };

        let client = async move {
            let stream = AsyncTcpStream::connect(addr).await.unwrap();
            drop(stream);
        };

        future::join(server, client).await;
    });
}
