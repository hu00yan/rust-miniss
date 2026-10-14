use bytes::Bytes;
use futures::channel::oneshot;
use futures::future::{self, Either};
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use rust_miniss::compat::FuturesAsyncReadCompatExt;
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::{MinissHyperExecutor, MinissHyperTimer, MultiCoreRuntime, Runtime};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

#[test]
fn hyper_http2_connection_runs_on_miniss() {
    let io_runtime = MultiCoreRuntime::new(Some(1)).expect("io runtime");
    let shutdown_runtime = io_runtime.clone();
    let app_runtime = Runtime::new();

    app_runtime.block_on(async move {
        let executor = MinissHyperExecutor::new(io_runtime.clone());
        let timer = MinissHyperTimer;
        let (client_done_tx, client_done_rx) = oneshot::channel();
        let (cancel_seen_tx, cancel_seen_rx) = oneshot::channel();
        let cancel_seen = Arc::new(parking_lot::Mutex::new(Some(cancel_seen_tx)));
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let cancel_seen = cancel_seen.clone();
            let service = service_fn(move |req: Request<Incoming>| {
                let path = req.uri().path().to_owned();
                let cancel_seen = cancel_seen.clone();
                async move {
                    assert!(matches!(
                        path.as_str(),
                        "/h2/one" | "/h2/two" | "/h2/large" | "/h2/error" | "/h2/cancel"
                    ));
                    if path == "/h2/cancel" {
                        let _ = req.into_body().collect().await;
                        if let Some(tx) = cancel_seen.lock().take() {
                            let _ = tx.send(());
                        }
                        return Ok::<_, Infallible>(
                            Response::builder()
                                .status(204)
                                .body(Full::new(Bytes::new()))
                                .unwrap(),
                        );
                    }
                    let body = if path == "/h2/large" {
                        Bytes::from(vec![b'x'; 32 * 1024])
                    } else if path == "/h2/error" {
                        Bytes::from_static(b"expected error")
                    } else {
                        Bytes::from_static(b"hello h2")
                    };
                    let status = if path == "/h2/error" { 500 } else { 200 };
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(status)
                            .body(Full::new(body))
                            .unwrap(),
                    )
                }
            });
            let mut builder = http2::Builder::new(executor);
            builder.timer(timer);
            let mut connection = Box::pin(builder.serve_connection(stream, service));
            let _ = future::select(connection.as_mut(), client_done_rx).await;
        };

        let client = async move {
            let stream = AsyncTcpStream::connect(addr).await.unwrap();
            let mut client_builder = h2::client::Builder::new();
            client_builder
                .initial_window_size(16 * 1024)
                .initial_connection_window_size(64 * 1024);
            let (mut sender, connection) = client_builder
                .handshake::<_, Bytes>(stream.compat())
                .await
                .unwrap();
            let request = |mut sender: h2::client::SendRequest<Bytes>,
                           path: &'static str,
                           status: u16,
                           expected: Bytes| async move {
                let request = Request::builder().uri(path).body(()).unwrap();
                let (response_future, _) = sender.send_request(request, true).unwrap();
                let response =
                    rust_miniss::timer::timeout(std::time::Duration::from_secs(2), response_future)
                        .await
                        .expect("HTTP/2 response headers timed out")
                        .unwrap();
                assert_eq!(response.status().as_u16(), status);
                let mut body = response.into_body();
                let mut data = Vec::new();
                while let Some(chunk) = body.data().await {
                    let chunk = chunk.unwrap();
                    let len = chunk.len();
                    data.extend_from_slice(&chunk);
                    body.flow_control().release_capacity(len).unwrap();
                }
                assert_eq!(data, expected);
            };
            let first = request(
                sender.clone(),
                "https://localhost/h2/one",
                200,
                Bytes::from_static(b"hello h2"),
            );
            let second = request(
                sender.clone(),
                "https://localhost/h2/two",
                200,
                Bytes::from_static(b"hello h2"),
            );
            let large = Bytes::from(vec![b'x'; 32 * 1024]);
            let third = request(sender.clone(), "https://localhost/h2/large", 200, large);
            let fourth = request(
                sender.clone(),
                "https://localhost/h2/error",
                500,
                Bytes::from_static(b"expected error"),
            );
            let request_task = async move {
                future::join4(first, second, third, fourth).await;
                let request = Request::builder()
                    .uri("https://localhost/h2/cancel")
                    .body(())
                    .unwrap();
                let (response, mut cancel_stream) = sender.send_request(request, false).unwrap();
                cancel_stream.send_reset(h2::Reason::CANCEL);
                assert!(response.await.is_err(), "reset stream returned a response");
                rust_miniss::timer::timeout(std::time::Duration::from_secs(2), cancel_seen_rx)
                    .await
                    .expect("HTTP/2 cancellation was not observed")
                    .expect("HTTP/2 cancellation notification dropped");
                let _ = client_done_tx.send(());
            };
            let mut connection = Box::pin(connection);
            match future::select(connection.as_mut(), Box::pin(request_task)).await {
                Either::Left((_connection, request_task)) => {
                    request_task.await;
                    let _ =
                        rust_miniss::timer::timeout(std::time::Duration::from_secs(2), connection)
                            .await
                            .expect("HTTP/2 peer close was not observed by the client");
                }
                Either::Right((_request_task, connection)) => {
                    let _ =
                        rust_miniss::timer::timeout(std::time::Duration::from_secs(2), connection)
                            .await
                            .expect("HTTP/2 peer close was not observed by the client");
                }
            }
        };

        rust_miniss::timer::timeout(
            std::time::Duration::from_secs(5),
            future::join(server, client),
        )
        .await
        .expect("HTTP/2 exchange timed out");
    });

    shutdown_runtime
        .shutdown_with_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    Arc::try_unwrap(shutdown_runtime)
        .expect("runtime still referenced")
        .join()
        .expect("worker join");
}
