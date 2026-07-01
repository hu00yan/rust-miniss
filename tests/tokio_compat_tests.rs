use futures::future;
use futures::io::{AsyncReadExt as FuturesAsyncReadExt, AsyncWriteExt as FuturesAsyncWriteExt};
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Runtime;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

#[test]
fn test_tokio_compat_read() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tokio_stream = stream.compat();

            let mut buffer = [0u8; 1024];
            let n = tokio_stream.read(&mut buffer).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buffer[..n], b"hello");

            tokio_stream.write_all(b"world").await.unwrap();
            tokio_stream.flush().await.unwrap();
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"hello").await.unwrap();
            stream.flush().await.unwrap();

            let mut response = [0u8; 1024];
            let n = stream.read(&mut response).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&response[..n], b"world");
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_tokio_compat_write() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tokio_stream = stream.compat();

            let mut buffer = [0u8; 1024];
            let n = tokio_stream.read(&mut buffer).await.unwrap();
            assert_eq!(n, 12);
            assert_eq!(&buffer[..n], b"tokio_compat");

            tokio_stream.write_all(b"ok").await.unwrap();
        };

        let client = async move {
            let stream = AsyncTcpStream::connect(addr).await.unwrap();
            let mut tokio_writer = stream.compat_write();
            tokio_writer.write_all(b"tokio_compat").await.unwrap();
            tokio_writer.flush().await.unwrap();

            let mut response = [0u8; 1024];
            let n = tokio_writer.get_mut().read(&mut response).await.unwrap();
            assert_eq!(n, 2);
            assert_eq!(&response[..n], b"ok");
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_tokio_futures_interop() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tokio_stream = stream.compat();

            let mut buffer = [0u8; 1024];
            loop {
                match tokio_stream.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => {
                        tokio_stream.write_all(&buffer[..n]).await.unwrap();
                        tokio_stream.flush().await.unwrap();
                    }
                    Err(e) => panic!("Server read error: {e}"),
                }
            }
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"echo_test").await.unwrap();
            stream.flush().await.unwrap();

            let mut response = [0u8; 1024];
            let n = stream.read(&mut response).await.unwrap();
            assert_eq!(n, 9);
            assert_eq!(&response[..n], b"echo_test");
        };

        future::join(server, client).await;
    });
}
