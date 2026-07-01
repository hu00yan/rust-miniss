use futures::future;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::net::AsyncTcpListener;
use rust_miniss::Runtime;
use std::net::SocketAddr;

#[test]
fn test_async_read_trait_implementation() {
    fn assert_async_read<T: futures::io::AsyncRead>() {}
    assert_async_read::<rust_miniss::AsyncTcpStream>();
}

#[test]
fn test_async_write_trait_implementation() {
    fn assert_async_write<T: futures::io::AsyncWrite>() {}
    assert_async_write::<rust_miniss::AsyncTcpStream>();
}

#[test]
fn test_tcp_read_from_server() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"data from server").await.unwrap();
            stream.flush().await.unwrap();
        };

        let client = async move {
            let mut stream = rust_miniss::net::AsyncTcpStream::connect(addr)
                .await
                .unwrap();
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"data from server");
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_tcp_write_from_client() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"client data");
        };

        let client = async move {
            let mut stream = rust_miniss::net::AsyncTcpStream::connect(addr)
                .await
                .unwrap();
            stream.write_all(b"client data").await.unwrap();
            stream.flush().await.unwrap();
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_poll_flush_returns_ok() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let client = async move {
            let mut stream = rust_miniss::net::AsyncTcpStream::connect(addr)
                .await
                .unwrap();
            stream.write_all(b"test").await.unwrap();
            stream.flush().await.unwrap();
        };

        let server = async move {
            let _ = listener.accept().await;
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_poll_close_returns_ok() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let client = async move {
            let mut stream = rust_miniss::net::AsyncTcpStream::connect(addr)
                .await
                .unwrap();
            stream.write_all(b"test").await.unwrap();
            stream.close().await.unwrap();
        };

        let server = async move {
            let _ = listener.accept().await;
        };

        future::join(server, client).await;
    });
}
