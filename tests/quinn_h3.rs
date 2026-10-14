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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn build_certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (
        cert.cert.into(),
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    )
}

fn quic_connection_count() -> usize {
    std::env::var("MINISS_QUIC_CONNECTIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&count| count > 0)
        .unwrap_or(32)
}

#[test]
fn quinn_and_h3_quinn_run_on_miniss() {
    let io_runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let app_runtime = Runtime::new();
    let shutdown_runtime = io_runtime.clone();
    let quic_runtime_source = io_runtime.clone();
    app_runtime.block_on(async move {
        let quic_runtime = Arc::new(MinissQuicRuntime::new(quic_runtime_source));
        let (response_done_tx, response_done_rx) = futures::channel::oneshot::channel();
        let (reset_seen_tx, reset_seen_rx) = futures::channel::oneshot::channel();
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
            // The client may have completed endpoint rebind before the
            // server-side accept future is polled.  Migration legitimately
            // changes the peer port; the later H3/datagram exchange proves
            // that the new path remains usable.
            assert_eq!(conn.remote_address().ip(), client_local.ip());

            let datagram_conn = conn.clone();
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
            let datagram =
                rust_miniss::timer::timeout(Duration::from_secs(1), datagram_conn.read_datagram())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(&datagram[..], b"quic datagram");
            let (_, mut reset_stream) =
                rust_miniss::timer::timeout(Duration::from_secs(1), datagram_conn.accept_bi())
                    .await
                    .unwrap()
                    .unwrap();
            let reset = reset_stream.read_to_end(1).await.unwrap_err();
            assert!(matches!(
                reset,
                quinn::ReadToEndError::Read(quinn::ReadError::Reset(code))
                    if code.into_inner() == 42
            ));
            let _ = reset_seen_tx.send(());
            let _ = response_done_rx.await;
            rust_miniss::timer::timeout(Duration::from_secs(1), datagram_conn.closed())
                .await
                .expect("QUIC connection close timed out");
        };

        let client_task = async move {
            let conn = client_endpoint
                .connect(server_local, "localhost")
                .unwrap()
                .await
                .unwrap();
            let rebound_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            let rebound_addr = rebound_socket.local_addr().unwrap();
            client_endpoint.rebind(rebound_socket).unwrap();
            assert_ne!(rebound_addr, client_local);
            conn.send_datagram(Bytes::from_static(b"quic datagram"))
                .unwrap();
            let reset_conn = conn.clone();
            let (mut driver, mut client) = client::new(H3QuinnConnection::new(conn)).await.unwrap();
            let driver_task = rust_miniss::timer::timeout(
                Duration::from_secs(1),
                future::poll_fn(|cx| driver.poll_close(cx)),
            );

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
                let (mut reset_stream, _) = reset_conn.open_bi().await.unwrap();
                reset_stream.reset(42u32.into()).unwrap();
                let _ = reset_seen_rx.await;
                reset_conn.close(0u32.into(), b"test close");
                let _ = response_done_tx.send(());
            };

            let (_, _) = future::join(driver_task, request_task).await;
        };

        rust_miniss::timer::timeout(
            Duration::from_secs(5),
            future::join(server_task, client_task),
        )
        .await
        .expect("QUIC/H3 exchange timed out");
    });

    shutdown_runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .unwrap();
}

#[test]
fn quinn_zero_rtt_is_accepted_on_miniss() {
    let io_runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let app_runtime = Runtime::new();
    let shutdown_runtime = io_runtime.clone();
    let quic_runtime_source = io_runtime.clone();

    app_runtime.block_on(async move {
        let quic_runtime = Arc::new(MinissQuicRuntime::new(quic_runtime_source));
        let (cert, key) = build_certs();
        let server_config = quinn::ServerConfig::with_single_cert(vec![cert.clone()], key).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();

        let server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_endpoint = quinn::Endpoint::new(
            Default::default(),
            Some(server_config),
            server_socket,
            quic_runtime.clone(),
        )
        .unwrap();
        let server_addr = server_endpoint.local_addr().unwrap();
        let client_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client_endpoint =
            quinn::Endpoint::new(Default::default(), None, client_socket, quic_runtime).unwrap();
        client_endpoint.set_default_client_config(client_config);

        let server_task = async move {
            let first = server_endpoint.accept().await.unwrap().accept().unwrap();
            let (first_conn, first_established) = first.into_0rtt().unwrap();
            let mut first_stream = first_conn.accept_uni().await.unwrap();
            assert_eq!(first_stream.read_to_end(64).await.unwrap(), b"first");
            let _ = first_established.await;
            let mut reply = first_conn.open_uni().await.unwrap();
            reply.write_all(b"ticket").await.unwrap();
            reply.finish().unwrap();
            rust_miniss::timer::sleep(Duration::from_millis(100)).await;
            drop(first_conn);

            let second = server_endpoint.accept().await.unwrap().accept().unwrap();
            let (second_conn, second_established) = second.into_0rtt().unwrap();
            let mut second_stream = second_conn.accept_uni().await.unwrap();
            assert!(second_stream.is_0rtt(), "second stream was not early data");
            assert_eq!(second_stream.read_to_end(64).await.unwrap(), b"early");
            let _ = second_established.await;
        };

        let client_task = async move {
            let first = client_endpoint
                .connect(server_addr, "localhost")
                .unwrap()
                .await
                .unwrap();
            let mut first_stream = first.open_uni().await.unwrap();
            first_stream.write_all(b"first").await.unwrap();
            first_stream.finish().unwrap();
            let mut reply = first.accept_uni().await.unwrap();
            assert_eq!(reply.read_to_end(64).await.unwrap(), b"ticket");
            drop(first);

            let connecting = client_endpoint.connect(server_addr, "localhost").unwrap();
            let (second, accepted) = connecting.into_0rtt().expect("0-RTT keys unavailable");
            let mut stream = second.open_uni().await.unwrap();
            stream.write_all(b"early").await.unwrap();
            stream.finish().unwrap();
            assert!(accepted.await);
        };

        rust_miniss::timer::timeout(
            Duration::from_secs(5),
            future::join(server_task, client_task),
        )
        .await
        .expect("QUIC 0-RTT exchange timed out");
    });

    shutdown_runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .unwrap();
}

#[test]
fn quinn_handshake_refusal_is_bounded_and_cleans_up() {
    let io_runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let app_runtime = Runtime::new();
    let shutdown_runtime = io_runtime.clone();
    let quic_runtime = Arc::new(MinissQuicRuntime::new(io_runtime.clone()));
    let (cert, key) = build_certs();

    let server_config = quinn::ServerConfig::with_single_cert(vec![cert.clone()], key).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
    let server_endpoint = quinn::Endpoint::new(
        Default::default(),
        Some(server_config),
        UdpSocket::bind("127.0.0.1:0").unwrap(),
        quic_runtime.clone(),
    )
    .unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();
    let mut client_endpoint = quinn::Endpoint::new(
        Default::default(),
        None,
        UdpSocket::bind("127.0.0.1:0").unwrap(),
        quic_runtime,
    )
    .unwrap();
    client_endpoint.set_default_client_config(client_config);
    app_runtime.block_on(async move {
        let server = async move {
            let incoming =
                rust_miniss::timer::timeout(Duration::from_secs(2), server_endpoint.accept())
                    .await
                    .expect("server did not receive handshake")
                    .expect("server endpoint closed");
            incoming.refuse();
            server_endpoint.close(0u32.into(), b"handshake refused");
        };
        let client = async move {
            let result = rust_miniss::timer::timeout(
                Duration::from_secs(5),
                client_endpoint
                    .connect(server_addr, "localhost")
                    .expect("client connect setup failed"),
            )
            .await;
            assert!(
                result.is_err(),
                "refused handshake unexpectedly completed successfully"
            );
        };

        rust_miniss::timer::timeout(Duration::from_secs(8), future::join(server, client))
            .await
            .expect("QUIC handshake refusal lifecycle timed out");
    });

    shutdown_runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .unwrap();
}

#[test]
fn quinn_peer_close_is_observed_by_the_client() {
    let io_runtime = MultiCoreRuntime::new(Some(1)).unwrap();
    let app_runtime = Runtime::new();
    let shutdown_runtime = io_runtime.clone();
    let quic_runtime = Arc::new(MinissQuicRuntime::new(io_runtime.clone()));
    let (cert, key) = build_certs();

    let server_config = quinn::ServerConfig::with_single_cert(vec![cert.clone()], key).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();

    let server_endpoint = quinn::Endpoint::new(
        Default::default(),
        Some(server_config),
        UdpSocket::bind("127.0.0.1:0").unwrap(),
        quic_runtime.clone(),
    )
    .unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();
    let mut client_endpoint = quinn::Endpoint::new(
        Default::default(),
        None,
        UdpSocket::bind("127.0.0.1:0").unwrap(),
        quic_runtime,
    )
    .unwrap();
    client_endpoint.set_default_client_config(client_config);

    app_runtime.block_on(async move {
        let server = async move {
            let incoming = server_endpoint.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            connection.close(42u32.into(), b"server shutdown");
            rust_miniss::timer::timeout(Duration::from_secs(1), connection.closed())
                .await
                .expect("server close did not complete");
        };
        let client = async move {
            let connection = client_endpoint
                .connect(server_addr, "localhost")
                .unwrap()
                .await
                .unwrap();
            rust_miniss::timer::timeout(Duration::from_secs(2), connection.closed())
                .await
                .expect("client did not observe peer close");
            assert!(
                connection.close_reason().is_some(),
                "client lost the peer close reason"
            );
        };

        rust_miniss::timer::timeout(Duration::from_secs(5), future::join(server, client))
            .await
            .expect("QUIC peer-close lifecycle timed out");
    });

    shutdown_runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .unwrap();
}

#[test]
fn quinn_endpoints_run_on_separate_owned_shards() {
    let connections = quic_connection_count();
    let io_timeout = if connections > 128 {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(2)
    };
    let operation_timeout = if connections > 128 {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(3)
    };
    let report_timeout = if connections > 128 {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(10)
    };
    let io_runtime = MultiCoreRuntime::new(Some(2)).unwrap();
    let app_runtime = Runtime::new();
    let shutdown_runtime = io_runtime.clone();
    let quic_runtime = Arc::new(MinissQuicRuntime::new(io_runtime.clone()));
    let (cert, key) = build_certs();

    let server_config = quinn::ServerConfig::with_single_cert(vec![cert.clone()], key).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
    let (addr_tx, addr_rx) = futures::channel::oneshot::channel();
    let (server_done_tx, server_done_rx) = futures::channel::oneshot::channel();
    let (client_done_tx, client_done_rx) = futures::channel::oneshot::channel();
    let server_read_done = Arc::new(AtomicBool::new(false));
    let server_read_done_for_server = server_read_done.clone();
    let server_read_done_for_client = server_read_done.clone();
    let client_runtime = io_runtime.clone();

    io_runtime
        .spawn_on(0, {
            let quic_runtime = quic_runtime.clone();
            async move {
                let result = async {
                    let socket = UdpSocket::bind("127.0.0.1:0")?;
                    let endpoint = quinn::Endpoint::new(
                        Default::default(),
                        Some(server_config),
                        socket,
                        quic_runtime,
                    )?;
                    addr_tx.send(endpoint.local_addr()?).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "address receiver")
                    })?;
                    let incomings = future::try_join_all((0..connections).map(|_| async {
                        rust_miniss::timer::timeout(io_timeout, endpoint.accept())
                            .await
                            .map_err(|_| {
                                std::io::Error::new(std::io::ErrorKind::TimedOut, "accept")
                            })?
                            .ok_or_else(|| {
                                std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    "endpoint closed",
                                )
                            })
                    }))
                    .await?;
                    let mut payloads =
                        future::try_join_all(incomings.into_iter().map(|incoming| async move {
                            let connection = rust_miniss::timer::timeout(
                                io_timeout,
                                incoming.accept().unwrap(),
                            )
                            .await
                            .map_err(|_| {
                                std::io::Error::new(std::io::ErrorKind::TimedOut, "handshake")
                            })??;
                            let mut stream = rust_miniss::timer::timeout(
                                io_timeout,
                                connection.accept_uni(),
                            )
                            .await
                            .map_err(|_| {
                                std::io::Error::new(std::io::ErrorKind::TimedOut, "stream accept")
                            })??;
                            let payload = rust_miniss::timer::timeout(
                                io_timeout,
                                stream.read_to_end(64),
                            )
                            .await
                            .map_err(|_| {
                                std::io::Error::new(std::io::ErrorKind::TimedOut, "stream read")
                            })?
                            .map_err(|error| std::io::Error::other(error.to_string()))?;
                            String::from_utf8(payload).map_err(|error| {
                                std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                            })
                        }))
                        .await?;
                    payloads.sort();
                    let mut expected = (0..connections)
                        .map(|index| format!("cross-shard-{index}"))
                        .collect::<Vec<_>>();
                    expected.sort();
                    if payloads != expected {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("unexpected QUIC payload set: got {payloads:?}, expected {expected:?}"),
                        ));
                    }
                    server_read_done_for_server.store(true, Ordering::Release);
                    endpoint.close(0u32.into(), b"test complete");
                    endpoint.wait_idle().await;
                    Ok::<_, std::io::Error>(())
                }
                .await;
                let _ = server_done_tx.send(result.map_err(|error| error.to_string()));
            }
        })
        .unwrap();

    io_runtime
        .spawn_on(1, async move {
            let result = async {
                let server_addr = addr_rx.await.map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "address sender")
                })?;
                let socket = UdpSocket::bind("127.0.0.1:0")?;
                let mut endpoint =
                    quinn::Endpoint::new(Default::default(), None, socket, quic_runtime).unwrap();
                endpoint.set_default_client_config(client_config);
                let mut completions = Vec::with_capacity(connections);
                for index in 0..connections {
                    // Avoid turning the test into a one-shot UDP handshake
                    // burst. Connections remain concurrent after launch; the
                    // small spacing makes packet loss reflect runtime
                    // behavior rather than host socket-buffer overflow.
                    if connections > 128 && index != 0 {
                        rust_miniss::timer::sleep(Duration::from_millis(1)).await;
                    }
                    let endpoint = endpoint.clone();
                    let server_read_done = server_read_done_for_client.clone();
                    let (done_tx, done_rx) = futures::channel::oneshot::channel();
                    client_runtime
                        .spawn_local(async move {
                            let operation = async move {
                                let connection = rust_miniss::timer::timeout(
                                    io_timeout,
                                    endpoint.connect(server_addr, "localhost").unwrap(),
                                )
                                .await
                                .map_err(|_| {
                                    std::io::Error::new(std::io::ErrorKind::TimedOut, "connect")
                                })??;
                                let mut stream = connection.open_uni().await?;
                                let payload = format!("cross-shard-{index}");
                                stream.write_all(payload.as_bytes()).await?;
                                stream.finish().unwrap();
                                // Keep the connection alive while the server-side
                                // concurrent readers observe the terminal stream.
                                while !server_read_done.load(Ordering::Acquire) {
                                    rust_miniss::timer::sleep(Duration::from_millis(10)).await;
                                }
                                Ok::<_, std::io::Error>(())
                            };
                            let result = rust_miniss::timer::timeout(operation_timeout, operation)
                                .await
                                .map_err(|_| {
                                    std::io::Error::new(
                                        std::io::ErrorKind::TimedOut,
                                        format!("client stream {index} operation timed out"),
                                    )
                                });
                            let _ = done_tx.send(result.map_err(|error| error.to_string()));
                        })
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                    completions.push(async move {
                        done_rx
                            .await
                            .map_err(|_| std::io::Error::other("client task dropped result"))?
                            .map_err(std::io::Error::other)
                    });
                }
                future::try_join_all(completions).await?;
                endpoint.close(0u32.into(), b"test complete");
                endpoint.wait_idle().await;
                Ok::<_, std::io::Error>(())
            }
            .await;
            let _ = client_done_tx.send(result.map_err(|error| error.to_string()));
        })
        .unwrap();

    let exchange_result = app_runtime.block_on(async {
        let server = rust_miniss::timer::timeout(report_timeout, server_done_rx)
            .await
            .map_err(|_| "server did not report within 5 seconds".to_owned())?
            .map_err(|_| "server task dropped its result".to_owned())?;
        if let Err(error) = server {
            return Err(format!("server task failed: {error}"));
        }

        let client = rust_miniss::timer::timeout(report_timeout, client_done_rx)
            .await
            .map_err(|_| "client did not report within 10 seconds".to_owned())?
            .map_err(|_| "client task dropped its result".to_owned())?;
        client.map_err(|error| format!("client task failed: {error}"))
    });

    let shutdown_result = shutdown_runtime.shutdown_with_timeout(Duration::from_secs(1));
    shutdown_result.expect("QUIC pressure runtime shutdown");
    exchange_result.expect("QUIC multi-connection exchange");
}
