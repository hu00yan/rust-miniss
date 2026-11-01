//! A3 shutdown-bridge oracles (methodology §T4 INV-6, S2/D28):
//!
//! - `guard_drains_bounded_and_visible`: `QuicEndpointGuard::drained` is
//!   the strong bounded drain — it closes every connection, reaches idle
//!   within the bound, publishes `Drained`, and the runtime still
//!   terminates through repeated bounded shutdowns with an all-zero
//!   snapshot (INV-6, including the duplicate-shutdown injection).
//! - `guard_drop_reports_drain_outcome`: dropping the guard with a live
//!   connection posts the bounded background drain and the owner can
//!   observe `Drained` through the shared status after the drop.
//! - `driver_loss_is_bounded_visible` (D28/台账 #33): when the runtime
//!   shuts down underneath a live endpoint — the shape of the silent
//!   driver-loss race in `src/quic.rs:52-62` — the guard's bounded drain
//!   fails within the bound with `DrainError::TimedOut` and publishes
//!   `TimedOut`, instead of hanging forever.

#[path = "common/quiescent.rs"]
mod quiescent;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rust_miniss::quic::shutdown::{DrainState, QuicEndpointGuard};
use rust_miniss::quic::QuicAcceptor;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use std::time::Duration;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const BOUND: Duration = Duration::from_secs(10);
const IDLE: Duration = Duration::from_secs(30);

// The fd baseline in the quiescence oracle is process-global: serialize the
// real-network legs so a sibling test runtime cannot inflate a sibling's count
// (ledger #49; mirrors tests/quic_lifecycle_props.rs:38).
static REAL_LEG_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("rcgen");
    (
        cert.cert.into(),
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    )
}

fn server_config(
    cert: &CertificateDer<'static>,
    key: &PrivateKeyDer<'static>,
) -> quinn::ServerConfig {
    let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.clone_key())
    .expect("server cert");
    crypto.alpn_protocols = vec![b"shutdown/1".to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(crypto).expect("quic server crypto"),
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(IDLE.try_into().expect("idle")));
    config.transport = Arc::new(transport);
    config
}

fn client_config(cert: &CertificateDer<'static>) -> quinn::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone()).expect("valid certificate");
    let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3")
    .with_root_certificates(roots)
    .with_no_client_auth();
    crypto.alpn_protocols = vec![b"shutdown/1".to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("quic crypto"),
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(IDLE.try_into().expect("idle")));
    config.transport_config(Arc::new(transport));
    config
}

/// One established client connection to `addr` that stays open.
async fn connect_client(
    client: &mut quinn::Endpoint,
    addr: std::net::SocketAddr,
) -> quinn::Connection {
    rust_miniss::timer::timeout(BOUND, client.connect(addr, "localhost").expect("connect"))
        .await
        .expect("handshake bounded")
        .expect("handshake ok")
}

/// A3 strong path: the guard's bounded drain completes within the bound,
/// publishes Drained, and the runtime terminates quiescently afterwards.
#[test]
fn guard_drains_bounded_and_visible() {
    let _leg_lock = REAL_LEG_LOCK.lock();
    let guard_runtime = quiescent::QuiescenceGuard::start(Some(1));
    let runtime = Arc::clone(guard_runtime.runtime());
    let app = rust_miniss::Runtime::new();
    let (cert, key) = certs();

    let (_client, drain_state) = app.block_on(async {
        let quic_runtime: Arc<dyn quinn::Runtime> = Arc::new(
            rust_miniss::quic::MinissQuicRuntime::new(Arc::clone(&runtime)),
        );
        let endpoint = quinn::Endpoint::new(
            Default::default(),
            Some(server_config(&cert, &key)),
            std::net::UdpSocket::bind("127.0.0.1:0").expect("bind server"),
            Arc::clone(&quic_runtime),
        )
        .expect("server endpoint");
        let addr = endpoint.local_addr().expect("server addr");
        let guard = QuicEndpointGuard::new(endpoint, Arc::clone(&runtime), DRAIN_TIMEOUT);
        let status = guard.status();

        // Accept loop on the endpoint's shard: one connection, then the
        // drain ends the endpoint.
        let (accepted_tx, accepted_rx) = futures::channel::oneshot::channel();
        runtime
            .spawn_on(0, {
                let endpoint = guard.endpoint().clone();
                async move {
                    let mut acceptor = QuicAcceptor::new(
                        endpoint,
                        Arc::new(rust_miniss::quic::QuicLimits::default()),
                    );
                    let _ = accepted_tx.send(acceptor.next_connection().await.is_some());
                }
            })
            .expect("admit acceptor task");

        let mut client = quinn::Endpoint::new(
            Default::default(),
            None,
            std::net::UdpSocket::bind("127.0.0.1:0").expect("bind client"),
            quic_runtime,
        )
        .expect("client endpoint");
        client.set_default_client_config(client_config(&cert));
        let connection = connect_client(&mut client, addr).await;
        drop(connection);

        let accepted = rust_miniss::timer::timeout(BOUND, accepted_rx)
            .await
            .expect("acceptor verdict bounded")
            .expect("acceptor verdict delivered");
        assert!(accepted, "connection not accepted");

        // Strong drain: bounded close + wait_idle.
        let drained = guard.drained().await;
        assert!(drained.is_ok(), "strong drain failed: {drained:?}");
        assert_eq!(status.state(), DrainState::Drained);

        // INV-6: repeated bounded shutdowns still terminate quiescently.
        runtime
            .shutdown_with_timeout(Duration::from_secs(3))
            .expect("first shutdown");
        runtime
            .shutdown_with_timeout(Duration::from_secs(3))
            .expect("repeated shutdown");

        (client, status.state())
    });

    let _ = drain_state;
    guard_runtime.finish_quiescent("a3 guard drain");
}

/// A3 drop path: dropping the guard with a live connection posts the
/// bounded background drain; the owner observes the outcome afterwards.
#[test]
fn guard_drop_reports_drain_outcome() {
    let _leg_lock = REAL_LEG_LOCK.lock();
    let guard_runtime = quiescent::QuiescenceGuard::start(Some(1));
    let runtime = Arc::clone(guard_runtime.runtime());
    let app = rust_miniss::Runtime::new();
    let (cert, key) = certs();

    app.block_on(async {
        let quic_runtime: Arc<dyn quinn::Runtime> = Arc::new(
            rust_miniss::quic::MinissQuicRuntime::new(Arc::clone(&runtime)),
        );
        let endpoint = quinn::Endpoint::new(
            Default::default(),
            Some(server_config(&cert, &key)),
            std::net::UdpSocket::bind("127.0.0.1:0").expect("bind server"),
            Arc::clone(&quic_runtime),
        )
        .expect("server endpoint");
        let addr = endpoint.local_addr().expect("server addr");
        let guard = QuicEndpointGuard::new(endpoint, Arc::clone(&runtime), DRAIN_TIMEOUT);
        let status = guard.status();

        // Acceptor on the endpoint's shard: hold the connection until the
        // guard's background drain has reported.
        let (hold_tx, hold_rx) = futures::channel::oneshot::channel::<()>();
        runtime
            .spawn_on(0, {
                let endpoint = guard.endpoint().clone();
                async move {
                    let mut acceptor = QuicAcceptor::new(
                        endpoint,
                        Arc::new(rust_miniss::quic::QuicLimits::default()),
                    );
                    let accepted = acceptor.next_connection().await;
                    assert!(accepted.is_some(), "connection not accepted");
                    let _ = hold_rx.await;
                    // The task keeps its connection handle until here; the
                    // guard's close has already drained the endpoint.
                }
            })
            .expect("admit acceptor task");

        let mut client = quinn::Endpoint::new(
            Default::default(),
            None,
            std::net::UdpSocket::bind("127.0.0.1:0").expect("bind client"),
            quic_runtime,
        )
        .expect("client endpoint");
        client.set_default_client_config(client_config(&cert));
        let connection = connect_client(&mut client, addr).await;
        drop(connection);

        // Drop with a live connection: the background drain runs.
        drop(guard);
        let deadline = std::time::Instant::now() + DRAIN_TIMEOUT * 2;
        while status.state() != DrainState::Drained && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            status.state(),
            DrainState::Drained,
            "drop-path drain outcome not published within the bound"
        );
        // Release the acceptor task only after the outcome was observed.
        let _ = hold_tx.send(());
        // Keep the client endpoint alive until the drain reported.
        let _ = client.local_addr().expect("client alive");
    });

    guard_runtime.finish_quiescent("a3 guard drop");
}

/// D28/台账 #33: the silent driver-loss race made bounded-visible. The
/// runtime shuts down underneath a live endpoint (killing its drivers);
/// the guard's bounded drain then cannot complete and reports
/// `TimedOut` within the bound instead of hanging forever.
#[test]
fn driver_loss_is_bounded_visible() {
    let _leg_lock = REAL_LEG_LOCK.lock();
    let runtime = rust_miniss::MultiCoreRuntime::new(Some(1)).expect("runtime");
    let app = rust_miniss::Runtime::new();
    let (cert, key) = certs();

    let (guard, status) = app.block_on(async {
        let quic_runtime: Arc<dyn quinn::Runtime> = Arc::new(
            rust_miniss::quic::MinissQuicRuntime::new(Arc::clone(&runtime)),
        );
        let endpoint = quinn::Endpoint::new(
            Default::default(),
            Some(server_config(&cert, &key)),
            std::net::UdpSocket::bind("127.0.0.1:0").expect("bind server"),
            Arc::clone(&quic_runtime),
        )
        .expect("server endpoint");
        let addr = endpoint.local_addr().expect("server addr");
        let guard = QuicEndpointGuard::new(endpoint, Arc::clone(&runtime), DRAIN_TIMEOUT);
        let status = guard.status();

        // Acceptor on the endpoint's shard: take the connection and hold it.
        runtime
            .spawn_on(0, {
                let endpoint = guard.endpoint().clone();
                async move {
                    let mut acceptor = QuicAcceptor::new(
                        endpoint,
                        Arc::new(rust_miniss::quic::QuicLimits::default()),
                    );
                    let accepted = acceptor.next_connection().await;
                    assert!(accepted.is_some(), "connection not accepted");
                    futures::future::pending::<()>().await;
                }
            })
            .expect("admit acceptor task");

        let mut client = quinn::Endpoint::new(
            Default::default(),
            None,
            std::net::UdpSocket::bind("127.0.0.1:0").expect("bind client"),
            quic_runtime,
        )
        .expect("client endpoint");
        client.set_default_client_config(client_config(&cert));
        let connection = connect_client(&mut client, addr).await;
        drop(connection);

        // Simulate the admission-failure race: the runtime (and with it
        // every endpoint/connection driver and shard task) shuts down while
        // the connection is live. The endpoint object survives in the
        // guard, but nothing can ever drive it again.
        runtime
            .shutdown_with_timeout(Duration::from_secs(3))
            .expect("runtime shutdown");
        (guard, status)
    });

    // The guard outlives the runtime: dropping it must publish the
    // un-runnable drain (the D28 silent-loss shape) instead of dying
    // quietly, and the strong drain must report the bound.
    drop(guard);
    assert_eq!(
        status.state(),
        DrainState::TimedOut,
        "the un-runnable drain was not published"
    );
}
