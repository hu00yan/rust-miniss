use rust_miniss::{net::AsyncTcpStream, Runtime};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

#[test]
fn test_tokio_compat_read() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tokio_stream = stream.compat();

            // Read using tokio::io AsyncRead
            let mut buffer = [0u8; 1024];
            let n = tokio_stream.read(&mut buffer).await.unwrap();

            assert_eq!(n, 5);
            assert_eq!(&buffer[..n], b"hello");

            // Write response using tokio::io AsyncWrite
            tokio_stream.write_all(b"world").await.unwrap();
            tokio_stream.flush().await.unwrap();
        });

        // Client uses our futures::io implementation directly
        use futures::io::{AsyncReadExt, AsyncWriteExt};
        let mut client_stream = AsyncTcpStream::connect(actual_addr).await.unwrap();

        client_stream.write_all(b"hello").await.unwrap();
        client_stream.flush().await.unwrap();

        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();

        assert_eq!(n, 5);
        assert_eq!(&response[..n], b"world");

        server_handle.unwrap().await;
    });
}

#[test]
fn test_tokio_compat_write() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tokio_stream = stream.compat();

            // Read using tokio::io
            let mut buffer = [0u8; 1024];
            let n = tokio_stream.read(&mut buffer).await.unwrap();
            assert_eq!(n, 12);
            assert_eq!(&buffer[..n], b"tokio_compat");

            tokio_stream.write_all(b"ok").await.unwrap();
        });

        let mut client_stream = AsyncTcpStream::connect(actual_addr).await.unwrap();
        let mut tokio_client = client_stream.compat_write();

        tokio_client.write_all(b"tokio_compat").await.unwrap();
        tokio_client.flush().await.unwrap();

        use futures::io::AsyncReadExt;
        let mut response = [0u8; 1024];
        let n = tokio_client.get_mut().read(&mut response).await.unwrap();

        assert_eq!(n, 2);
        assert_eq!(&response[..n], b"ok");

        server_handle.unwrap().await;
    });
}

#[test]
fn test_tokio_futures_interop() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        // Server uses tokio, client uses futures
        let server_handle = rust_miniss::task::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tokio_stream = stream.compat();

            // Echo server using tokio
            let mut buffer = [0u8; 1024];
            loop {
                match tokio_stream.read(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        tokio_stream.write_all(&buffer[..n]).await.unwrap();
                        tokio_stream.flush().await.unwrap();
                    }
                    Err(e) => panic!("Server read error: {}", e),
                }
            }
        });

        // Client uses futures::io
        use futures::io::{AsyncReadExt, AsyncWriteExt};
        let mut client_stream = AsyncTcpStream::connect(actual_addr).await.unwrap();

        // Send message
        client_stream.write_all(b"echo_test").await.unwrap();
        client_stream.flush().await.unwrap();

        // Read echo
        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();

        assert_eq!(n, 9);
        assert_eq!(&response[..n], b"echo_test");

        // Close connection
        drop(client_stream);

        server_handle.unwrap().await;
    });
}
