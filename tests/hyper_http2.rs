use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::{MinissHyperExecutor, MinissHyperTimer, MultiCoreRuntime, Runtime};
use std::convert::Infallible;
use std::net::SocketAddr;

#[test]
fn hyper_http2_connection_runs_on_miniss() {
    let io_runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let app_runtime = Runtime::new();
    let shutdown_runtime = io_runtime.clone();

    app_runtime.block_on(async move {
        let executor = MinissHyperExecutor::new(io_runtime.clone());
        let timer = MinissHyperTimer;
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let server_addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        io_runtime
            .spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();

                let service = service_fn(|req: Request<Incoming>| async move {
                    assert_eq!(req.uri().path(), "/h2");
                    let body = Full::new(Bytes::from_static(b"hello h2"));
                    Ok::<_, Infallible>(Response::new(body))
                });

                let mut builder = hyper::server::conn::http2::Builder::new(executor);
                builder.timer(timer);
                let mut http = Box::pin(builder.serve_connection(stream, service));
                tokio::select! {
                    res = &mut http => {
                        res.unwrap();
                    }
                    _ = shutdown_rx => {
                        http.as_mut().graceful_shutdown();
                        let _ = http.await;
                    }
                }
                let _ = done_tx.send(());
            })
            .unwrap();

        let stream = AsyncTcpStream::connect(server_addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, Empty<Bytes>>(
            MinissHyperExecutor::new(io_runtime.clone()),
            stream,
        )
        .await
        .unwrap();

        io_runtime
            .spawn(async move {
                let _ = conn.await;
            })
            .unwrap();

        let response = sender
            .send_request(
                Request::builder()
                    .uri("http://localhost/h2")
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, Bytes::from_static(b"hello h2"));
        drop(sender);
        let _ = shutdown_tx.send(());

        done_rx.await.unwrap();
    });

    shutdown_runtime.shutdown().unwrap();
}
