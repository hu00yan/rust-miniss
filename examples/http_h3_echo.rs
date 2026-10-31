//! HTTP/3 (QUIC, RFC 9114) echo server on the miniss runtime.
//!
//! QUIC transport is provided by `quinn` driven by `MinissQuicRuntime`
//! (`src/quic.rs`), and the HTTP/3 layer by `h3`/`h3-quinn` — the same
//! combination exercised by `tests/quinn_h3.rs`. Each request gets the same
//! 22-byte payload as the other echo servers.
//!
//! Topology: one SO_REUSEPORT UDP socket + one quinn endpoint per worker core.
//! The endpoint is constructed *on* its shard, so quinn's connection driver is
//! spawned with `spawn_local` (`MinissQuicRuntime::spawn`) and never migrates;
//! accepted H3 connections and request streams stay on that shard too.
//!
//! TLS is a self-signed "localhost" certificate generated at startup (rcgen).
//! `--selftest` runs an in-process H3 client against the server, prints
//! `h3 selftest ok` and exits — useful where no HTTP/3 curl/load tool exists.
//!
//! Usage: `http_h3_echo [--addr 127.0.0.1:8082] [--cores N] [--selftest]`

use bytes::{Buf, Bytes};
use h3::client;
use h3::server;
use h3_quinn::Connection as H3QuinnConnection;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Endpoint, ServerConfig};
use rcgen::generate_simple_self_signed;
use rust_miniss::multicore::MultiCoreRuntime;
use rust_miniss::quic::MinissQuicRuntime;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

/// Same 22-byte payload as the minimal-framer `http_echo.rs` response.
const RESPONSE_BODY: &[u8] = b"Hello from rust-miniss";

const SERVER_NAME: &str = "localhost";

fn build_certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = generate_simple_self_signed(vec![SERVER_NAME.to_owned()])
        .expect("self-signed certificate generation");
    (
        cert.cert.into(),
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    )
}

fn h3_server_config(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> ServerConfig {
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3 is supported by the ring provider")
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .expect("server certificate/key pair is valid");
    server_crypto.alpn_protocols = vec![b"h3".to_vec()];

    let mut config = ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(server_crypto).expect("QUIC server crypto config"),
    ));
    config.transport = Arc::new(quinn::TransportConfig::default());
    config
}

fn h3_client_config(cert: CertificateDer<'static>) -> quinn::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).expect("root certificate is valid");
    let mut client_crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3 is supported by the ring provider")
    .with_root_certificates(roots)
    .with_no_client_auth();
    client_crypto.alpn_protocols = vec![b"h3".to_vec()];
    quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(client_crypto).expect("QUIC client crypto config"),
    ))
}

/// UDP socket with SO_REUSEPORT so every worker can own an endpoint on the
/// same address (kernel-dispatch model, matching the TCP echo examples).
fn bind_reuse_port_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_port(true)?;
    socket.bind(&addr.into())?;
    // `into()` after bind keeps the socket blocking; miniss's QUIC wrapper
    // (MinissUdpSocket::new) sets non-blocking itself.
    Ok(socket.into())
}

async fn serve_h3_connection(runtime: Arc<MultiCoreRuntime>, connection: quinn::Connection) {
    let mut h3_conn = match server::Connection::new(H3QuinnConnection::new(connection)).await {
        Ok(conn) => conn,
        Err(_) => return,
    };
    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                // Each request stream is handled as its own shard-local task so
                // concurrent requests on one connection make progress.
                if runtime.spawn_local(handle_h3_request(resolver)).is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

async fn handle_h3_request(resolver: server::RequestResolver<H3QuinnConnection, Bytes>) {
    let (_request, mut stream) = match resolver.resolve_request().await {
        Ok(resolved) => resolved,
        Err(_) => return,
    };
    let response = http::Response::builder()
        .status(200)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(())
        .expect("static echo response is always valid");
    if stream.send_response(response).await.is_ok() {
        let _ = stream.send_data(Bytes::from_static(RESPONSE_BODY)).await;
    }
    let _ = stream.finish().await;
}

fn main() -> io::Result<()> {
    let mut addr = String::from("127.0.0.1:8082");
    let mut cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut selftest = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" | "-a" => {
                if let Some(v) = args.next() {
                    addr = v;
                }
            }
            "--cores" | "-c" => {
                if let Some(v) = args.next() {
                    cores = v.parse::<usize>().map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid --cores value {v:?}: {e}"),
                        )
                    })?;
                }
            }
            "--selftest" => selftest = true,
            _ => {}
        }
    }

    if cores == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--cores must be greater than zero",
        ));
    }

    let addr: SocketAddr = addr.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid listen address: {e}"),
        )
    })?;

    // Optional quinn/miniss tracing for diagnostics (RUST_LOG=debug etc.).
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt::try_init();
    }

    let (cert, key) = build_certs();
    let server_config = h3_server_config(cert.clone(), key);
    let client_config = h3_client_config(cert);

    let runtime = MultiCoreRuntime::new(Some(cores))
        .map_err(|e| io::Error::other(format!("failed to create runtime: {e}")))?;
    let quic_runtime = Arc::new(MinissQuicRuntime::new(runtime.clone()));

    let (endpoint_ready_tx, endpoint_ready_rx) = std::sync::mpsc::channel();
    for core_id in 0..cores {
        let worker_runtime = runtime.clone();
        let quic_runtime = quic_runtime.clone();
        let server_config = server_config.clone();
        let endpoint_ready_tx = endpoint_ready_tx.clone();
        runtime
            .spawn_on(core_id, async move {
                // Bind and construct the endpoint ON this shard so quinn's
                // driver task is spawn_local'd here and stays here.
                let socket = match bind_reuse_port_udp(addr) {
                    Ok(socket) => socket,
                    Err(e) => {
                        let _ = endpoint_ready_tx
                            .send(Err(format!("udp bind error on core {core_id}: {e}")));
                        eprintln!("udp bind error on core {core_id}: {e}");
                        return;
                    }
                };
                let endpoint = match Endpoint::new(
                    Default::default(),
                    Some(server_config),
                    socket,
                    quic_runtime,
                ) {
                    Ok(endpoint) => {
                        let _ = endpoint_ready_tx.send(Ok(endpoint.local_addr().unwrap_or(addr)));
                        endpoint
                    }
                    Err(e) => {
                        let _ = endpoint_ready_tx
                            .send(Err(format!("endpoint error on core {core_id}: {e}")));
                        eprintln!("endpoint error on core {core_id}: {e}");
                        return;
                    }
                };

                println!(
                    "rust-miniss http_h3_echo (quinn + h3) listening on {addr} with {cores} worker cores"
                );
                while let Some(incoming) = endpoint.accept().await {
                    let connection = match incoming.accept() {
                        Ok(connecting) => connecting,
                        Err(_) => break,
                    };
                    let connection = match connection.await {
                        Ok(connection) => connection,
                        Err(_) => continue,
                    };
                    if worker_runtime
                        .spawn_local(serve_h3_connection(worker_runtime.clone(), connection))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|e| io::Error::other(format!("failed to spawn accept loop: {e}")))?;
    }
    drop(endpoint_ready_tx);

    let mut ready_count = 0usize;
    let mut server_addr = None;
    for _ in 0..cores {
        match endpoint_ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(local_addr)) => {
                ready_count += 1;
                server_addr = Some(local_addr);
            }
            Ok(Err(error)) => {
                let _ = runtime.shutdown();
                return Err(io::Error::new(io::ErrorKind::AddrInUse, error));
            }
            Err(error) => {
                let _ = runtime.shutdown();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for all worker endpoints: {error}"),
                ));
            }
        }
    }
    if ready_count != cores || server_addr.is_none() {
        let _ = runtime.shutdown();
        return Err(io::Error::other("missing worker endpoint ack"));
    }
    let server_addr = server_addr.expect("at least one endpoint reported");

    if selftest {
        // The client endpoint must be constructed on a shard: quinn spawns an
        // endpoint driver and a per-connection driver through
        // `MinissQuicRuntime::spawn`, and a socket may only be polled from the
        // shard that owns it. Building the endpoint on the main thread would
        // let those two drivers land on different shards and fail with
        // "QUIC socket owned by shard X, polled from shard Y".
        let (result_tx, result_rx) = futures::channel::oneshot::channel();
        runtime
            .spawn_on(0, async move {
                let result = async {
                    let socket = UdpSocket::bind("127.0.0.1:0")
                        .map_err(|e| format!("client udp bind: {e}"))?;
                    let mut endpoint =
                        Endpoint::new(Default::default(), None, socket, quic_runtime)
                            .map_err(|e| format!("client endpoint: {e}"))?;
                    endpoint.set_default_client_config(client_config);

                    let connection = rust_miniss::timer::timeout(
                        Duration::from_secs(5),
                        endpoint
                            .connect(server_addr, SERVER_NAME)
                            .map_err(|e| format!("client connect: {e}"))?,
                    )
                    .await
                    .map_err(|_| "H3 selftest connect timed out".to_owned())?
                    .map_err(|e| format!("H3 selftest connect failed: {e}"))?;
                    let (mut driver, mut client) = client::new(H3QuinnConnection::new(connection))
                        .await
                        .map_err(|e| format!("H3 client init: {e}"))?;
                    let driver_task = futures::future::poll_fn(|cx| driver.poll_close(cx));
                    let request_task = async move {
                        let mut request_stream = client
                            .send_request(
                                http::Request::get("https://localhost/h3").body(()).unwrap(),
                            )
                            .await
                            .map_err(|e| format!("H3 request send: {e}"))?;
                        request_stream
                            .finish()
                            .await
                            .map_err(|e| format!("H3 request finish: {e}"))?;
                        let response = request_stream
                            .recv_response()
                            .await
                            .map_err(|e| format!("H3 response: {e}"))?;
                        assert_eq!(response.status(), 200);
                        let body = request_stream
                            .recv_data()
                            .await
                            .map_err(|e| format!("H3 body: {e}"))?
                            .expect("H3 body chunk missing");
                        assert_eq!(body.chunk(), RESPONSE_BODY);
                        assert!(request_stream.recv_data().await.unwrap().is_none());
                        Ok::<(), String>(())
                    };
                    let outcome = rust_miniss::timer::timeout(
                        Duration::from_secs(10),
                        futures::future::join(driver_task, request_task),
                    )
                    .await
                    .map_err(|_| "H3 selftest exchange timed out".to_owned())?;
                    outcome.1
                }
                .await;
                let _ = result_tx.send(result);
            })
            .map_err(|e| io::Error::other(format!("failed to spawn selftest: {e}")))?;
        runtime.block_on(async move {
            match result_rx.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => panic!("H3 selftest failed: {error}"),
                Err(_) => panic!("H3 selftest task was dropped"),
            }
        });
        println!("h3 selftest ok");
        let _ = runtime.shutdown();
        return Ok(());
    }

    runtime.block_on(futures::future::pending::<()>());
    let _ = runtime.shutdown();
    Ok(())
}
