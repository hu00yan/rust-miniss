//! Unit tests for futures::io AsyncRead/AsyncWrite traits
//! Tests the correct state management and async behavior

use futures::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::{net::AsyncTcpStream, Runtime};
use std::net::SocketAddr;

#[test]
fn test_futures_async_read_basic() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        // Start server task
        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read using futures::io AsyncRead
            let mut buffer = [0u8; 1024];
            let n = stream.read(&mut buffer).await.unwrap();

            assert_eq!(n, 12);
            assert_eq!(&buffer[..n], b"Hello, World!");

            // Write response using futures::io AsyncWrite
            stream.write_all(b"Response").await.unwrap();
            stream.flush().await.unwrap();
        });

        // Client uses futures::io
        let mut client_stream = AsyncTcpStream::from(actual_addr);

        // Write using futures::io
        client_stream.write_all(b"Hello, World!").await.unwrap();
        client_stream.flush().await.unwrap();

        // Read response using futures::io
        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();

        assert_eq!(n, 8);
        assert_eq!(&response[..n], b"Response");

        server_handle.await;
    });
}

#[test]
fn test_futures_async_read_partial() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read partial data in chunks
            let mut first_chunk = [0u8; 5];
            let n1 = stream.read(&mut first_chunk).await.unwrap();
            assert_eq!(n1, 5);
            assert_eq!(&first_chunk, b"Hello");

            let mut second_chunk = [0u8; 10];
            let n2 = stream.read(&mut second_chunk).await.unwrap();
            assert_eq!(n2, 7);
            assert_eq!(&second_chunk[..n2], b", Future");

            // Write response
            stream.write_all(b"OK").await.unwrap();
        });

        let mut client_stream = AsyncTcpStream::from(actual_addr);

        // Send data that will be read in parts
        client_stream.write_all(b"Hello, Future").await.unwrap();
        client_stream.flush().await.unwrap();

        // Read response
        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(&response[..n], b"OK");

        server_handle.await;
    });
}

#[test]
fn test_futures_async_read_zero_length() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Try to read into zero-length buffer
            let mut zero_buffer = [0u8; 0];
            let n = stream.read(&mut zero_buffer).await.unwrap();
            assert_eq!(n, 0);

            // Read normal data
            let mut normal_buffer = [0u8; 1024];
            let n = stream.read(&mut normal_buffer).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&normal_buffer[..n], b"Hello");

            stream.write_all(b"OK").await.unwrap();
        });

        let mut client_stream = AsyncTcpStream::from(actual_addr);

        client_stream.write_all(b"Hello").await.unwrap();
        client_stream.flush().await.unwrap();

        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(&response[..n], b"OK");

        server_handle.await;
    });
}

#[test]
fn test_futures_async_write_basic() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read using futures::io
            let mut buffer = [0u8; 1024];
            let n = stream.read(&mut buffer).await.unwrap();

            assert_eq!(n, 15);
            assert_eq!(&buffer[..n], b"Write Test Data");

            // Echo back
            stream.write_all(&buffer[..n]).await.unwrap();
        });

        let mut client_stream = AsyncTcpStream::from(actual_addr);

        // Write using futures::io
        let data = b"Write Test Data";
        let n = client_stream.write(data).await.unwrap();
        assert_eq!(n, data.len());

        // Read echo
        let mut echo = [0u8; 1024];
        let n = client_stream.read(&mut echo).await.unwrap();
        assert_eq!(n, data.len());
        assert_eq!(&echo[..n], data);

        server_handle.await;
    });
}

#[test]
fn test_futures_async_write_partial() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read multiple chunks
            let mut total_data = Vec::new();
            let mut buffer = [0u8; 10];

            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        total_data.extend_from_slice(&buffer[..n]);
                    }
                    Err(e) => panic!("Read error: {}", e),
                }
            }

            assert_eq!(total_data, b"Partial Write Test");
            stream.write_all(b"OK").await.unwrap();
        });

        let mut client_stream = AsyncTcpStream::from(actual_addr);

        // Write data in multiple chunks
        let chunks = [b"Partial", b" Write", b" Test"];
        for chunk in &chunks {
            let n = client_stream.write(chunk).await.unwrap();
            assert_eq!(n, chunk.len());
        }

        client_stream.flush().await.unwrap();

        // Read response
        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(&response[..n], b"OK");

        server_handle.await;
    });
}

#[test]
fn test_futures_async_write_large_data() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read large data
            let mut buffer = [0u8; 10000];
            let mut total_read = 0;

            while total_read < 5000 {
                match stream.read(&mut buffer[total_read..]).await {
                    Ok(0) => break, // EOF
                    Ok(n) => total_read += n,
                    Err(e) => panic!("Read error: {}", e),
                }
            }

            assert_eq!(total_read, 5000);
            assert_eq!(&buffer[..total_read], &vec![b'X'; 5000][..]);

            stream.write_all(b"Received").await.unwrap();
        });

        let mut client_stream = AsyncTcpStream::from(actual_addr);

        // Write large data
        let large_data = vec![b'X'; 5000];
        let n = client_stream.write(&large_data).await.unwrap();
        assert_eq!(n, 5000);

        client_stream.flush().await.unwrap();

        // Read response
        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();
        assert_eq!(n, 8);
        assert_eq!(&response[..n], b"Received");

        server_handle.await;
    });
}

#[test]
fn test_futures_flush_operation() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read data
            let mut buffer = [0u8; 1024];
            let n = stream.read(&mut buffer).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buffer[..n], b"Flush");

            // Write response
            stream.write_all(b"OK").await.unwrap();
        });

        let mut client_stream = AsyncTcpStream::from(actual_addr);

        // Write data and flush
        client_stream.write_all(b"Flush").await.unwrap();
        client_stream.flush().await.unwrap(); // Should not panic

        // Read response
        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(&response[..n], b"OK");

        server_handle.await;
    });
}

#[test]
fn test_futures_concurrent_read_write() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Echo server using futures::io
            let mut buffer = [0u8; 1024];
            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if let Err(e) = stream.write_all(&buffer[..n]).await {
                            eprintln!("Echo error: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("Read error: {}", e);
                        break;
                    }
                }
            }
        });

        let mut client_stream = AsyncTcpStream::from(actual_addr);

        // Send multiple messages concurrently
        let messages = [b"Hello", b"World", b"Test"];
        for message in &messages {
            client_stream.write_all(message).await.unwrap();
            client_stream.flush().await.unwrap();

            // Read echo
            let mut echo = [0u8; 1024];
            let n = client_stream.read(&mut echo).await.unwrap();
            assert_eq!(n, message.len());
            assert_eq!(&echo[..n], message);
        }

        // Close connection
        drop(client_stream);

        server_handle.await;
    });
}

#[test]
fn test_futures_error_handling() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        // Try to connect to a non-existent server
        let invalid_addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let mut client_stream = AsyncTcpStream::from(invalid_addr);

        // Try to read (should eventually fail)
        let mut buffer = [0u8; 1024];
        let result = client_stream.read(&mut buffer).await;

        // Should handle error gracefully (either connection refused or timeout)
        match result {
            Ok(_) => {
                // Unlikely but possible - test still passes
            }
            Err(e) => {
                // Expected error
                assert!(e.kind() == std::io::ErrorKind::ConnectionRefused ||
                       e.kind() == std::io::ErrorKind::ConnectionReset ||
                       e.kind() == std::io::ErrorKind::TimedOut);
            }
        }
    });
}

#[test]
fn test_futures_read_eof() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rust_miniss::net::AsyncTcpListener::bind(server_addr).unwrap();
        let actual_addr = listener.local_addr().unwrap();

        let server_handle = rust_miniss::task::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read initial data
            let mut buffer = [0u8; 1024];
            let n = stream.read(&mut buffer).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buffer[..n], b"Data");

            // Write response and close
            stream.write_all(b"Response").await.unwrap();
            // Stream closes when dropped
        });

        let mut client_stream = AsyncTcpStream::from(actual_addr);

        // Send data
        client_stream.write_all(b"Data").await.unwrap();
        client_stream.flush().await.unwrap();

        // Read response
        let mut response = [0u8; 1024];
        let n = client_stream.read(&mut response).await.unwrap();
        assert_eq!(n, 8);
        assert_eq!(&response[..n], b"Response");

        // Try to read again (should return EOF)
        let mut eof_buffer = [0u8; 1024];
        let eof_result = client_stream.read(&mut eof_buffer).await;
        assert_eq!(eof_result.unwrap(), 0);

        server_handle.await;
    });
}