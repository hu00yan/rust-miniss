use bytes::Bytes;
use futures::channel::oneshot;
use futures::future;
use http::{Request, Response};
use http_body_util::Full;
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
        let (served_tx, served_rx) = oneshot::channel();
        let served_tx = Arc::new(parking_lot::Mutex::new(Some(served_tx)));
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(move |req: Request<Incoming>| {
                let served_tx = Arc::clone(&served_tx);
                async move {
                    assert_eq!(req.uri().path(), "/h2");
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"hello h2"))))
                        .inspect(|_| {
                            if let Some(sender) = served_tx.lock().unwrap().take() {
                                let _ = sender.send(());
                            }
                        })
                }
            });
            let mut builder = http2::Builder::new(executor);
            builder.timer(timer);
            let mut connection = Box::pin(builder.serve_connection(stream, service));
            let _ = future::select(connection.as_mut(), served_rx).await;
        };

        let client = async move {
            let stream = AsyncTcpStream::connect(addr).await.unwrap();
            let (mut sender, connection) = h2::client::handshake(stream.compat()).await.unwrap();
            let request_task = async move {
                let request = Request::builder()
                    .uri("https://localhost/h2")
                    .body(())
                    .unwrap();
                let (response_future, _) = sender.send_request(request, true).unwrap();
                let response = response_future.await.unwrap();
                assert_eq!(response.status(), 200);
                let mut body = response.into_body();
                let data = body.data().await.unwrap().unwrap();
                assert_eq!(&data[..], b"hello h2");
            };
            let (_, _) = future::join(connection, request_task).await;
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
