use futures::io::AsyncReadExt;
use rust_miniss::{net::AsyncTcpListener, HttpConnection, Request, Response, Runtime, StatusCode};
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;

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
            conn.send_response(
                Response::new(StatusCode::OK)
                    .with_header("content-type", "text/plain")
                    .with_body("hello"),
            )
            .await
            .unwrap();
        };

        let client = async move {
            let mut stream = rust_miniss::net::AsyncTcpStream::connect(addr)
                .await
                .unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            assert!(buf.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(buf.ends_with(b"\r\n\r\nhello"));
        };

        futures::future::join(server, client).await;
    });
}
