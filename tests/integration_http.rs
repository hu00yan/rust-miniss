use futures::future;
use futures::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "linux")]
use rust_miniss::Buffer;
use rust_miniss::{
    net::{AsyncTcpListener, AsyncTcpStream},
    HttpConnection, Request, Response, Runtime, StatusCode,
};
use std::io;
use std::net::SocketAddr;

async fn read_response(stream: &mut AsyncTcpStream) -> io::Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        let (n, response) = stream.read_rent(Buffer::with_capacity(4096)).await?;
        let bytes = response[..n].to_vec();
        response.recycle();
        Ok(bytes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut response = [0u8; 4096];
        let n = stream.read(&mut response).await?;
        Ok(response[..n].to_vec())
    }
}

#[test]
fn test_http_echo_integration() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = AsyncTcpListener::bind(addr).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = HttpConnection::new(stream);
            let result = rust_miniss::timer::timeout(
                std::time::Duration::from_secs(1),
                connection.read_request(),
            )
            .await
            .expect("malformed request handling timed out");
            assert!(result.is_err());
        };
        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream.write_all(b"GET\r\n\r\n").await.unwrap();
        };

        future::join(server, client).await;
    });
}

#[test]
fn test_http_partial_request_eof_is_reported_and_connection_finishes() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = HttpConnection::new(stream);
            let result = rust_miniss::timer::timeout(
                std::time::Duration::from_secs(1),
                connection.read_request(),
            )
            .await
            .expect("partial HTTP request handling timed out");
            assert!(result.is_err(), "partial request EOF was accepted");
        };
        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /incomplete HTTP/1.1\r\nhost: localhost\r\n")
                .await
                .unwrap();
            stream.close().await.unwrap();
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(2),
            futures::future::join(server, client),
        )
        .await
        .expect("partial HTTP request lifecycle timed out");
    });
}

#[test]
fn test_http_oversized_headers_are_rejected_and_connection_finishes() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = HttpConnection::new(stream);
            let result = rust_miniss::timer::timeout(
                std::time::Duration::from_secs(1),
                connection.read_request(),
            )
            .await
            .expect("oversized header handling timed out");
            assert!(result.is_err(), "oversized headers were accepted");
        };
        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            let request = format!(
                "GET / HTTP/1.1\r\nx-overflow: {}\r\n\r\n",
                "x".repeat(64 * 1024)
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            stream.close().await.unwrap();
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(2),
            futures::future::join(server, client),
        )
        .await
        .expect("oversized HTTP header lifecycle timed out");
    });
}

#[test]
fn test_http_conflicting_content_length_is_rejected_and_connection_finishes() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = HttpConnection::new(stream);
            let result = rust_miniss::timer::timeout(
                std::time::Duration::from_secs(1),
                connection.read_request(),
            )
            .await
            .expect("conflicting Content-Length handling timed out");
            assert!(result.is_err(), "conflicting Content-Length was accepted");
        };
        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream
                .write_all(
                    b"POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\na",
                )
                .await
                .unwrap();
            stream.close().await.unwrap();
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(2),
            futures::future::join(server, client),
        )
        .await
        .expect("conflicting Content-Length lifecycle timed out");
    });
}

#[test]
fn test_http_incomplete_body_eof_is_rejected_and_connection_finishes() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = HttpConnection::new(stream);
            let error = rust_miniss::timer::timeout(
                std::time::Duration::from_secs(1),
                connection.read_request(),
            )
            .await
            .expect("incomplete HTTP body handling timed out")
            .expect_err("incomplete body was accepted");
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        };
        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 5\r\n\r\nabc")
                .await
                .unwrap();
            stream.close().await.unwrap();
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(2),
            futures::future::join(server, client),
        )
        .await
        .expect("incomplete HTTP body lifecycle timed out");
    });
}

#[test]
fn test_http_transfer_encoding_is_rejected_and_connection_finishes() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = HttpConnection::new(stream);
            let error = rust_miniss::timer::timeout(
                std::time::Duration::from_secs(1),
                connection.read_request(),
            )
            .await
            .expect("Transfer-Encoding handling timed out")
            .expect_err("unsupported Transfer-Encoding was accepted");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        };
        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream
                .write_all(
                    b"POST / HTTP/1.1\r\nhost: localhost\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
                )
                .await
                .unwrap();
            stream.close().await.unwrap();
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(2),
            futures::future::join(server, client),
        )
        .await
        .expect("Transfer-Encoding lifecycle timed out");
    });
}

#[test]
fn test_http_invalid_header_name_is_rejected_before_body_wait() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = HttpConnection::new(stream);
            let error = rust_miniss::timer::timeout(
                std::time::Duration::from_secs(1),
                connection.read_request(),
            )
            .await
            .expect("invalid header handling timed out")
            .expect_err("invalid header name was accepted");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        };
        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"POST / HTTP/1.1\r\nbad name: 1\r\ncontent-length: 100\r\n\r\n")
                .await
                .unwrap();
            stream.close().await.unwrap();
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(2),
            futures::future::join(server, client),
        )
        .await
        .expect("invalid header lifecycle timed out");
    });
}

#[test]
fn test_futures_io_hyper_compat() {
    let request = Request::parse(b"GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n").unwrap();
    let response = Response::new(StatusCode::OK).with_body("ok").to_bytes();

    assert_eq!(request.path, "/health");
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with(b"\r\n\r\nok"));
}

#[test]
fn test_http_send_response_vectored() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = AsyncTcpListener::bind(addr).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = HttpConnection::new(stream);
            for index in 0..2 {
                let request = conn.read_request().await.unwrap();
                if index == 0 {
                    assert_eq!(request.path, "/one");
                    assert_eq!(request.body, b"abc");
                } else {
                    assert_eq!(request.path, "/two");
                }
                conn.send_response(
                    Response::new(StatusCode::OK)
                        .with_header("content-type", "text/plain")
                        .with_body("hello"),
                )
                .await
                .unwrap();
            }
        };

        let client = async move {
            let mut stream = rust_miniss::net::AsyncTcpStream::connect(addr)
                .await
                .unwrap();
            stream
                .write_all(b"POST /one HTTP/1.1\r\nhost: localhost\r\ncontent-length: 3\r\n\r\nabc")
                .await
                .unwrap();
            let response = read_response(&mut stream).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(response.ends_with(b"\r\n\r\nhello"));

            stream
                .write_all(b"GET /two HTTP/1.1\r\nhost: localhost\r\n\r\n")
                .await
                .unwrap();
            let response = read_response(&mut stream).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(response.ends_with(b"\r\n\r\nhello"));
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(5),
            futures::future::join(server, client),
        )
        .await
        .expect("HTTP connection exchange timed out");
    });
}

#[test]
fn test_http_pipelined_requests_are_drained_from_one_connection() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = AsyncTcpListener::bind(addr).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = HttpConnection::new(stream);
            for expected_path in ["/one", "/two"] {
                let request = rust_miniss::timer::timeout(
                    std::time::Duration::from_secs(1),
                    connection.read_request(),
                )
                .await
                .expect("pipelined request handling timed out")
                .unwrap();
                assert_eq!(request.path, expected_path);
                connection
                    .send_response(Response::new(StatusCode::OK).with_body("hello"))
                    .await
                    .unwrap();
            }
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream
                .write_all(
                    b"GET /one HTTP/1.1\r\nhost: localhost\r\n\r\n\
                      GET /two HTTP/1.1\r\nhost: localhost\r\n\r\n",
                )
                .await
                .unwrap();
            stream.close().await.unwrap();

            let mut responses = Vec::new();
            stream.read_to_end(&mut responses).await.unwrap();
            assert_eq!(
                responses
                    .windows(5)
                    .filter(|part| *part == b"hello")
                    .count(),
                2
            );
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(5),
            futures::future::join(server, client),
        )
        .await
        .expect("pipelined HTTP lifecycle timed out");
    });
}
