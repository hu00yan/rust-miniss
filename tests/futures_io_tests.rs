use futures::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::{net::AsyncTcpStream, Runtime};
use std::net::SocketAddr;

#[test]
fn test_futures_async_read() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Create a server and client connection
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        // Start server task
        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read using futures::io AsyncRead
            let mut buffer = [0u8; 1024];
            let n = stream.read(&mut buffer).await.unwrap();

            assert_eq!(n, 5);
            assert_eq!(&buffer[..n], b"hello");

            // Write response using futures::io AsyncWrite
            stream.write_all(b"world").await.unwrap();
            stream.flush().await.unwrap();
        }).unwrap();

        // Connect client
        let mut client_stream = AsyncTcpStream::connect(actual_addr).await.unwrap();

        // Write using futures::io AsyncWrite
        client_stream.write_all(b"hello").await.unwrap();
        client_stream.flush().await.unwrap();

        // Read response using futures::io AsyncRead
        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();

        assert_eq!(n, 5);
        assert_eq!(&response[..n], b"world");

        server_handle.await;
    });
}

#[test]
fn test_futures_async_write() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read multiple chunks
            let mut buffer = [0u8; 1024];
            let mut total_read = 0;

            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(n) => total_read += n,
                    Err(e) => panic!("Read error: {}", e),
                }
            }

            assert_eq!(total_read, 10); // "hello" + "world"
        }).unwrap();

        // Client writes multiple times
        let mut client_stream = AsyncTcpStream::connect(actual_addr).await.unwrap();

        client_stream.write(b"hello").await.unwrap();
        client_stream.write(b"world").await.unwrap();
        client_stream.flush().await.unwrap();

        // Close the connection to signal EOF
        drop(client_stream);

        server_handle.await;
    });
}

#[test]
fn test_futures_partial_reads() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read in small chunks
            let mut first_chunk = [0u8; 3];
            let n1 = stream.read(&mut first_chunk).await.unwrap();
            assert_eq!(n1, 3);
            assert_eq!(&first_chunk, b"hel");

            let mut second_chunk = [0u8; 10];
            let n2 = stream.read(&mut second_chunk).await.unwrap();
            assert_eq!(n2, 2);
            assert_eq!(&second_chunk[..n2], b"lo");

            // Write response
            stream.write_all(b"response").await.unwrap();
        }).unwrap();

        let mut client_stream = AsyncTcpStream::connect(actual_addr).await.unwrap();
        client_stream.write_all(b"hello").await.unwrap();

        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();
        assert_eq!(n, 8);
        assert_eq!(&response[..n], b"response");

        server_handle.await;
    });
}
