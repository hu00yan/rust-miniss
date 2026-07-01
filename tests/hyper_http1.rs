use bytes::Bytes;
use futures::future;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Runtime;
use std::convert::Infallible;
use std::net::SocketAddr;

#[test]
fn hyper_http1_connection_runs_on_miniss() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut http = http1::Builder::new();
            http.keep_alive(false);

            let service = service_fn(|_req: Request<Incoming>| async move {
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"hello"))))
            });

            http.serve_connection(stream, service).await.unwrap();
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();

            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(response.ends_with(b"\r\n\r\nhello"));
        };

        future::join(server, client).await;
    });
}
