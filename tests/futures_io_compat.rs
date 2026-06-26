use futures::future;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::net::AsyncTcpListener;
use rust_miniss::Runtime;
use std::net::SocketAddr;

#[test]
fn test_futures_io_compat_read_write() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"compat reply").await.unwrap();
            stream.flush().await.unwrap();
        };

        let client = async move {
            let mut stream = rust_miniss::net::AsyncTcpStream::connect(addr)
                .await
                .unwrap();
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"compat reply");
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_futures_io_compat_ping_pong() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ping");
            stream.write_all(b"pong").await.unwrap();
            stream.flush().await.unwrap();
        };

        let client = async move {
            let mut stream = rust_miniss::net::AsyncTcpStream::connect(addr)
                .await
                .unwrap();
            stream.write_all(b"ping").await.unwrap();
            stream.flush().await.unwrap();

            let mut response = [0u8; 64];
            let n = stream.read(&mut response).await.unwrap();
            assert_eq!(&response[..n], b"pong");
        };

        future::join(server, client).await;
    });
}
