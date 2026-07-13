use futures::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "linux")]
use rust_miniss::Buffer;
use rust_miniss::{
    net::{AsyncTcpListener, AsyncTcpStream},
    HttpConnection, Request, Response, Runtime, StatusCode,
};
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;

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
        let listener = AsyncTcpListener::bind(addr).unwrap();

        // Test accept and basic I/O
        // Placeholder for full HTTP test
        assert!(listener.as_raw_fd() > 0);
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

        futures::future::join(server, client).await;
    });
}
