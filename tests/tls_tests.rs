#[cfg(feature = "tls")]
mod tls_tests {
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use rcgen::generate_simple_self_signed;
    use rust_miniss::net::AsyncTcpStream;
    use rust_miniss::tls::TlsStream;
    use rust_miniss::Runtime as MinissRuntime;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::future::Future;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Duration;

    const CLIENT_TIMEOUT: Duration = Duration::from_secs(4);

    // Every TLS client block_on in this module inherits the explicit deadline.
    type Runtime = BoundedRuntime;

    struct BoundedRuntime(MinissRuntime);

    impl BoundedRuntime {
        fn new() -> Self {
            Self(MinissRuntime::new())
        }

        fn block_on<F: Future>(&self, future: F) -> F::Output {
            self.0
                .block_on(rust_miniss::timer::timeout(CLIENT_TIMEOUT, future))
                .unwrap_or_else(|_| panic!("TLS client flow exceeded its 4-second bound"))
        }
    }

    #[test]
    fn test_tls_compiles() {
        static_assertions::assert_not_impl_any!(rust_miniss::tls::TlsStream: Send, Sync);
    }

    fn tls_configs(trust_server: bool) -> (rustls::ServerConfig, rustls::ClientConfig) {
        tls_configs_for_names(vec!["localhost".into()], trust_server)
    }

    fn tls_configs_for_names(
        names: Vec<String>,
        trust_server: bool,
    ) -> (rustls::ServerConfig, rustls::ClientConfig) {
        let cert = generate_simple_self_signed(names).unwrap();
        let certificate: CertificateDer<'static> = cert.cert.into();
        let key = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key)
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        if trust_server {
            roots.add(certificate).unwrap();
        }
        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (server_config, client_config)
    }

    fn spawn_tls_server(
        listener: TcpListener,
        server_config: rustls::ServerConfig,
        exchange: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::time::timeout(Duration::from_secs(5), async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let (stream, _) = listener.accept().await.unwrap();
                    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
                    let result = acceptor.accept(stream).await;
                    if exchange {
                        let mut stream = result.unwrap();
                        let mut request = [0; 4];
                        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut request)
                            .await
                            .unwrap();
                        assert_eq!(&request, b"ping");
                        tokio::io::AsyncWriteExt::write_all(&mut stream, b"pong")
                            .await
                            .unwrap();
                        tokio::io::AsyncWriteExt::shutdown(&mut stream)
                            .await
                            .unwrap();
                    } else {
                        assert!(result.is_err());
                    }
                })
                .await
                .expect("TLS server timed out");
            });
        })
    }

    fn spawn_plain_close_server(listener: TcpListener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::time::timeout(Duration::from_secs(5), async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let (stream, _) = listener.accept().await.unwrap();
                    drop(stream);
                })
                .await
                .expect("plain peer close server timed out");
            });
        })
    }

    fn spawn_tls_peer_close_server(
        listener: TcpListener,
        server_config: rustls::ServerConfig,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::time::timeout(Duration::from_secs(5), async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let (stream, _) = listener.accept().await.unwrap();
                    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
                    let stream = acceptor.accept(stream).await.unwrap();
                    drop(stream);
                })
                .await
                .expect("TLS peer-close server timed out");
            });
        })
    }

    #[test]
    fn tls_client_handshakes_reads_writes_and_closes() {
        let (server_config, client_config) = tls_configs(true);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = spawn_tls_server(listener, server_config, true);

        let runtime = Runtime::new();
        runtime.block_on(async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let mut stream = TlsStream::connect_with_config("localhost", tcp, client_config)
                .await
                .unwrap();
            stream.write_all(b"ping").await.unwrap();
            stream.flush().await.unwrap();
            let mut response = [0; 4];
            stream.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"pong");
            stream.close().await.unwrap();
        });

        server.join().expect("TLS server thread panicked");
    }

    #[test]
    fn tls_client_accepts_a_trusted_ip_subject_alt_name() {
        let (server_config, client_config) = tls_configs_for_names(vec!["127.0.0.1".into()], true);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = spawn_tls_server(listener, server_config, true);

        let runtime = Runtime::new();
        runtime.block_on(async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let mut stream = TlsStream::connect_with_config("127.0.0.1", tcp, client_config)
                .await
                .unwrap();
            stream.write_all(b"ping").await.unwrap();
            let mut response = [0; 4];
            stream.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"pong");
            stream.close().await.unwrap();
        });

        server.join().expect("TLS IP SAN server thread panicked");
    }

    #[test]
    fn tls_client_rejects_an_untrusted_server_certificate() {
        let (server_config, client_config) = tls_configs(false);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = spawn_tls_server(listener, server_config, false);

        let runtime = Runtime::new();
        runtime.block_on(async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let result = TlsStream::connect_with_config("localhost", tcp, client_config).await;
            assert!(result.is_err());
        });

        server.join().expect("TLS server thread panicked");
    }

    #[test]
    fn tls_default_root_store_rejects_a_private_certificate() {
        let (server_config, _) = tls_configs(true);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = spawn_tls_server(listener, server_config, false);

        let runtime = Runtime::new();
        runtime.block_on(async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let result = TlsStream::connect("localhost", tcp).await;
            assert!(result.is_err(), "bundled roots trusted a private test CA");
        });

        server.join().expect("TLS server thread panicked");
    }

    #[test]
    fn tls_client_rejects_a_trusted_certificate_for_the_wrong_name() {
        let (server_config, client_config) = tls_configs(true);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = spawn_tls_server(listener, server_config, false);

        let runtime = Runtime::new();
        runtime.block_on(async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let result = TlsStream::connect_with_config("wrong.example", tcp, client_config).await;
            assert!(result.is_err());
        });

        server.join().expect("TLS server thread panicked");
    }

    #[test]
    fn tls_client_reports_peer_eof_during_handshake() {
        let (_, client_config) = tls_configs(true);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = spawn_plain_close_server(listener);

        let runtime = Runtime::new();
        runtime.block_on(async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let result = TlsStream::connect_with_config("localhost", tcp, client_config).await;
            assert!(result.is_err(), "TLS handshake accepted peer EOF");
        });

        server.join().expect("plain peer close server panicked");
    }

    #[test]
    fn tls_client_reports_peer_eof_after_handshake() {
        let (server_config, client_config) = tls_configs(true);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = spawn_tls_peer_close_server(listener, server_config);

        let runtime = Runtime::new();
        runtime.block_on(async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let mut stream = TlsStream::connect_with_config("localhost", tcp, client_config)
                .await
                .unwrap();
            let mut byte = [0u8; 1];
            let error = stream.read(&mut byte).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        });

        server.join().expect("TLS peer-close server panicked");
    }

    #[test]
    fn tls_client_rejects_an_invalid_server_name_before_handshake() {
        let (_, client_config) = tls_configs(true);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            listener
                .accept()
                .expect("client connected for name validation");
        });

        let runtime = Runtime::new();
        runtime.block_on(async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let error =
                match TlsStream::connect_with_config("not a server name", tcp, client_config).await
                {
                    Ok(_) => panic!("invalid server name reached the handshake"),
                    Err(error) => error,
                };
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        });

        server.join().expect("plain listener thread panicked");
    }
}
