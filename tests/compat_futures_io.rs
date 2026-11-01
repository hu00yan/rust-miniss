use futures::future;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Runtime;
use std::net::SocketAddr;

#[test]
fn test_futures_io_traits() {
    fn assert_async_read<T: futures::io::AsyncRead>() {}
    fn assert_async_write<T: futures::io::AsyncWrite>() {}
    assert_async_read::<AsyncTcpStream>();
    assert_async_write::<AsyncTcpStream>();
}

#[test]
fn test_futures_io_ping_pong() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
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
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"ping").await.unwrap();
            stream.flush().await.unwrap();

            let mut response = [0u8; 64];
            let n = stream.read(&mut response).await.unwrap();
            assert_eq!(&response[..n], b"pong");
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_futures_io_partial_reads() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut first_chunk = [0u8; 5];
            stream.read_exact(&mut first_chunk).await.unwrap();
            assert_eq!(&first_chunk, b"hello");

            let mut second_chunk = [0u8; 5];
            stream.read_exact(&mut second_chunk).await.unwrap();
            assert_eq!(&second_chunk, b"world");

            stream.write_all(b"response").await.unwrap();
            stream.flush().await.unwrap();
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"helloworld").await.unwrap();
            stream.flush().await.unwrap();

            let mut response = [0u8; 8];
            stream.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"response");
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_futures_io_multi_write_drain() {
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
            drop(stream);
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_futures_io_close_signals_eof() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, b"closing-data");
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"closing-data").await.unwrap();
            stream.close().await.unwrap();
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_futures_io_drop_notifies_peer() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
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
