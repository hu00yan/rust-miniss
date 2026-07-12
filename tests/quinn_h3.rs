use bytes::{Buf, Bytes};
use futures::future;
use h3::client;
use h3::server;
use h3_quinn::Connection as H3QuinnConnection;
use http::{Request, Response};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rcgen::generate_simple_self_signed;
use rust_miniss::quic::MinissQuicRuntime;
use rust_miniss::{MultiCoreRuntime, Runtime};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::UdpSocket;
use std::sync::Arc;

fn build_certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (
        cert.cert.into(),
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    )
}

#[test]
#[ignore = "QUIC/H3 integration needs a bounded Linux-only harness; it can outlive the test runner on macOS"]
fn quinn_and_h3_quinn_run_on_miniss() {
    let io_runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let app_runtime = Runtime::new();
    let shutdown_runtime = io_runtime.clone();
    let quic_runtime_source = io_runtime.clone();

    app_runtime.block_on(async move {
        let quic_runtime = Arc::new(MinissQuicRuntime::new(quic_runtime_source));
        let (cert, key) = build_certs();

        let mut server_crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.clone_key())
        .unwrap();
        server_crypto.max_early_data_size = u32::MAX;
        server_crypto.alpn_protocols = vec![b"h3".to_vec()];

        let mut client_roots = rustls::RootCertStore::empty();
        client_roots.add(cert.clone()).unwrap();
        let mut client_crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(client_roots)
        .with_no_client_auth();
        client_crypto.enable_early_data = true;
        client_crypto.alpn_protocols = vec![b"h3".to_vec()];

        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(server_crypto).unwrap(),
        ));
        server_config.transport = Arc::new(quinn::TransportConfig::default());
        let client_config =
            quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto).unwrap()));

        let server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_endpoint = quinn::Endpoint::new(
            Default::default(),
            Some(server_config),
            server_socket,
            quic_runtime.clone(),
        )
        .unwrap();
        let server_local = server_endpoint.local_addr().unwrap();

        let client_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client_endpoint = quinn::Endpoint::new(
            Default::default(),
            None,
            client_socket,
            quic_runtime.clone(),
        )
        .unwrap();
        client_endpoint.set_default_client_config(client_config);
        let client_local = client_endpoint.local_addr().unwrap();

        let server_task = async move {
            let conn = server_endpoint.accept().await.unwrap().await.unwrap();
            assert_eq!(conn.remote_address(), client_local);

            let mut conn = server::Connection::new(H3QuinnConnection::new(conn))
                .await
                .unwrap();
            let request_resolver = conn.accept().await.unwrap().unwrap();
            let (request, mut stream) = request_resolver.resolve_request().await.unwrap();
            assert_eq!(request.uri().path(), "/h3");

            stream
                .send_response(Response::builder().status(200).body(()).unwrap())
                .await
                .unwrap();
            stream
                .send_data(Bytes::from_static(b"hello h3"))
                .await
                .unwrap();
            stream.finish().await.unwrap();
            let close = conn.accept().await.err().unwrap();
            assert!(close.is_h3_no_error());
        };

        let client_task = async move {
            let conn = client_endpoint
                .connect(server_local, "localhost")
                .unwrap()
                .await
                .unwrap();
            let (mut driver, mut client) = client::new(H3QuinnConnection::new(conn)).await.unwrap();
            let driver_task = async {
                let _ = future::poll_fn(|cx| driver.poll_close(cx)).await;
            };

            let request_task = async move {
                let mut request_stream = client
                    .send_request(Request::get("https://localhost/h3").body(()).unwrap())
                    .await
                    .unwrap();
                request_stream.finish().await.unwrap();
                let response = request_stream.recv_response().await.unwrap();
                assert_eq!(response.status(), 200);
                let body = request_stream.recv_data().await.unwrap().unwrap();
                assert_eq!(body.chunk(), b"hello h3");
                assert!(request_stream.recv_data().await.unwrap().is_none());
            };

            future::join(driver_task, request_task).await;
        };

        future::join(server_task, client_task).await;
    });

    shutdown_runtime.shutdown().unwrap();
}
