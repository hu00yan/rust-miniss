use futures::future;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::{
    net::{AsyncTcpListener, AsyncTcpStream},
    Runtime,
};
use std::net::SocketAddr;

#[test]
fn test_futures_async_read() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            let mut buffer = [0u8; 1024];
            let n = stream.read(&mut buffer).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buffer[..n], b"hello");

            stream.write_all(b"world").await.unwrap();
            stream.flush().await.unwrap();
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
fn test_futures_async_write() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            let mut total_read = 0usize;

            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(n) => total_read += n,
                    Err(e) => panic!("Read error: {e}"),
                }
            }

            assert_eq!(total_read, 10);
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"hello").await.unwrap();
            stream.write_all(b"world").await.unwrap();
            stream.flush().await.unwrap();
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_futures_partial_reads() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            let mut first_chunk = [0u8; 3];
            stream.read_exact(&mut first_chunk).await.unwrap();
            assert_eq!(&first_chunk, b"hel");

            let mut second_chunk = [0u8; 2];
            stream.read_exact(&mut second_chunk).await.unwrap();
            assert_eq!(&second_chunk, b"lo");

            stream.write_all(b"response").await.unwrap();
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"hello").await.unwrap();

            let mut response = [0u8; 8];
            let n = stream.read(&mut response).await.unwrap();
            assert_eq!(n, 8);
            assert_eq!(&response[..n], b"response");
        };

        future::join(server, client).await;
    });
}
