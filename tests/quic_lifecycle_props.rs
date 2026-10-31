//! T4 property core (methodology §4): proptest `Schedule` generators ×
//! invariant oracles, sim-flavor and real-flavor.
//!
//! Sim-flavor runs the Schedule over the quinn-seam simulator
//! (`common/quic_sim`) — single thread, virtual clock, seeded faults; any
//! failure shrinks to a minimal Schedule whose Debug form replays
//! verbatim (`cargo test quic_lifecycle_props`, optional
//! `MINISS_SIM_SEED=<n>` seed override).
//!
//! Real-flavor runs the same Schedule against the current production
//! adapter (`src/quic.rs`) on real loopback under `MinissQuicRuntime`,
//! with the quiescence oracle from `tests/common/quiescent.rs`.
//!
//! Invariants landed here (methodology §4.2): INV-1 (resource balance),
//! INV-3 (byte integrity), INV-5 (retirement conservation via the
//! termination-snapshot ledger), INV-6 (bounded liveness incl. repeated
//! shutdown), DIFF-1 (leg differential), PROBE (environment validity).
//! INV-2 lands with A1 (cap/refuse), INV-4 with A2 (stats fold), INV-7
//! with A5 (memory bound) per methodology §6.

#[path = "common/quic_sim/mod.rs"]
mod quic_sim;
#[path = "common/quiescent.rs"]
mod quiescent;

use proptest::prelude::*;
use quic_sim::{SimFaultSchedule, SimNetwork};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::Endpoint;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use std::time::Duration;

// The fd baseline in the quiescence oracle is process-global: serialize the
// real-flavor legs so a sibling runtime cannot inflate a sibling's count
// (the same test-serialisation helper pattern tests/metrics_aggregation.rs
// uses — a parking_lot lock, not runtime state).
static REAL_LEG_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

// ─── Schedule generator (methodology §4.1) ─────────────────────────────

/// Where a generated client drops its endpoint/connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropPoint {
    MidHandshake,
    MidTransfer,
}

/// One churn op, applied at a fixed virtual milestone per index.
#[derive(Debug, Clone)]
enum Op {
    /// Generator's `idle-advance`: let timers run for this long.
    IdleAdvanceMs(u64),
    /// The (index mod connections)-th client drops at the given point.
    DropClient { index: usize, at: DropPoint },
    /// Close every endpoint, twice (A3.5/D27 duplicate-shutdown dimension).
    ShutdownDup,
}

/// The shrunk value: every dimension the oracles judge.
#[derive(Debug, Clone)]
struct Schedule {
    connections: usize,
    streams_per_conn: usize,
    payload: usize,
    /// Stateless-retry policy dimension (consumed by the A1 acceptor).
    use_retry: bool,
    faults: SimFaultSchedule,
    /// Reverse the transfer direction (server writes: send-path-biased leg).
    reverse: bool,
    ops: Vec<Op>,
}

impl Schedule {
    /// Deterministic payload byte for (connection, stream, offset).
    fn byte(connection: usize, stream: usize, offset: usize) -> u8 {
        connection
            .wrapping_mul(31)
            .wrapping_add(stream.wrapping_mul(7))
            .wrapping_add(offset) as u8
    }
}

/// Self-describing stream body: `conn u32 LE | stream u32 LE | bytes…`.
/// The reader learns which generator flow a stream belongs to without any
/// shared mapping, and dropped streams still validate as prefixes.
fn framed_payload(connection: usize, stream: usize, payload: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload);
    frame.extend_from_slice(&(connection as u32).to_le_bytes());
    frame.extend_from_slice(&(stream as u32).to_le_bytes());
    frame.extend((0..payload).map(|offset| Schedule::byte(connection, stream, offset)));
    frame
}

fn parse_frame(bytes: &[u8]) -> Option<(usize, usize, &[u8])> {
    if bytes.len() < 8 {
        return None;
    }
    let connection = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    let stream = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
    Some((connection, stream, &bytes[8..]))
}

fn arb_faults() -> impl Strategy<Value = SimFaultSchedule> {
    (
        any::<u64>(),
        0.0f64..=0.2,
        0.0f64..=0.2,
        prop::option::of(0u64..=2000),
        1u64..=6,
        prop::option::of(1250usize..=1500),
    )
        .prop_map(
            |(seed, loss, dup, reorder_us, delay_base_ms, mtu)| SimFaultSchedule {
                seed,
                loss,
                dup,
                reorder: reorder_us.map(Duration::from_micros),
                delay: (
                    Duration::from_millis(delay_base_ms),
                    Duration::from_millis(1),
                ),
                partition: Vec::new(),
                mtu,
            },
        )
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u64..=300).prop_map(Op::IdleAdvanceMs),
        (
            any::<usize>(),
            prop::sample::select(vec![DropPoint::MidHandshake, DropPoint::MidTransfer,])
        )
            .prop_map(|(index, at)| Op::DropClient { index, at }),
        Just(Op::ShutdownDup),
    ]
}

fn arb_schedule() -> impl Strategy<Value = Schedule> {
    (
        0usize..=5,
        0usize..=4,
        0usize..=4096,
        any::<bool>(),
        arb_faults(),
        any::<bool>(),
        prop::collection::vec(arb_op(), 0..=4),
    )
        .prop_map(
            |(connections, streams_per_conn, payload, use_retry, mut faults, reverse, ops)| {
                if let Ok(seed) = std::env::var("MINISS_SIM_SEED")
                    .map_err(|_| ())
                    .and_then(|v| v.parse().map_err(|_| ()))
                {
                    faults.seed = seed;
                }
                Schedule {
                    connections,
                    streams_per_conn,
                    payload,
                    use_retry,
                    faults,
                    reverse,
                    ops,
                }
            },
        )
}

// ─── Shared oracles (methodology §4.2) ─────────────────────────────────

/// INV-3, prefix form: a server-side read from a possibly-dropped flow
/// must be a strict prefix of the generated frame (any length, incl. full).
fn check_prefix(received: &[u8], context: &str) -> Result<(), String> {
    let Some((connection, stream, body)) = parse_frame(received) else {
        return Err(format!(
            "INV-3: {context}: frame truncated at {} bytes",
            received.len()
        ));
    };
    let expected: Vec<u8> = (0..body.len())
        .map(|offset| Schedule::byte(connection, stream, offset))
        .collect();
    if body == expected.as_slice() {
        Ok(())
    } else {
        Err(format!(
            "INV-3: {context}: conn {connection} stream {stream}: corruption in prefix"
        ))
    }
}

/// INV-3, exact form: a completed observation must equal the full frame.
fn check_exact(received: &[u8], payload: usize, context: &str) -> Result<(), String> {
    let Some((connection, stream, body)) = parse_frame(received) else {
        return Err(format!(
            "INV-3: {context}: frame truncated at {} bytes",
            received.len()
        ));
    };
    if body.len() != payload {
        return Err(format!(
            "INV-3: {context}: conn {connection} stream {stream}: {} bytes, expected {payload}",
            body.len()
        ));
    }
    let expected: Vec<u8> = (0..payload)
        .map(|offset| Schedule::byte(connection, stream, offset))
        .collect();
    if body == expected.as_slice() {
        Ok(())
    } else {
        Err(format!(
            "INV-3: {context}: conn {connection} stream {stream}: completed transfer corrupted"
        ))
    }
}

/// Everything the scenario observed, shared across connection tasks.
#[derive(Default)]
struct Recorder {
    /// Server-side reads: validated under the prefix rule (the peer may
    /// have dropped mid-transfer).
    server_reads: parking_lot::Mutex<Vec<Vec<u8>>>,
    /// Client-side echo/data observations: validated under the exact rule.
    client_reads: parking_lot::Mutex<Vec<Vec<u8>>>,
    /// Connection attempts that failed (handshake or stream phase).
    failed: parking_lot::Mutex<usize>,
}

impl Recorder {
    fn fail(&self) {
        *self.failed.lock() += 1;
    }
}

/// INV-1 (sim half): the virtual world closes its books and drains.
fn assert_sim_drained(net: &SimNetwork, context: &str) {
    let counters = net.counters();
    assert_eq!(
        counters.packets_submitted + counters.duplicates_emitted,
        counters.packets_delivered
            + counters.dropped_loss
            + counters.dropped_mtu
            + counters.dropped_partition
            + counters.dropped_unreachable,
        "{context}: sim packet books do not close: {counters:?}"
    );
    assert!(
        net.is_quiescent(),
        "{context}: sim world left residue: counters={counters:?}"
    );
}

// ─── Sim-flavor scenario (quinn over SimNetwork) ───────────────────────

const IDLE_TIMEOUT: Duration = Duration::from_millis(1500);
const SIM_OP_MILESTONE: Duration = Duration::from_millis(120);
const STREAM_CAP_SLACK: usize = 24;

fn bounded_transport(idle: Duration) -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(idle.try_into().expect("idle timeout")));
    Arc::new(transport)
}

fn run_sim(schedule: &Schedule) {
    let net = SimNetwork::new(schedule.faults.clone());
    let recorder = Arc::new(Recorder::default());
    {
        let task_net = net.clone();
        let spawn_net = task_net.clone();
        let schedule = schedule.clone();
        let recorder = Arc::clone(&recorder);
        task_net.spawn(async move {
            run_sim_scenario(spawn_net, &schedule, recorder).await;
        });
    }

    let report = net.advance(Duration::from_secs(30));
    assert!(
        !report.exhausted,
        "seed {}: schedule exhausted the executor: {report:?}",
        schedule.faults.seed
    );

    // INV-3 verdicts.
    for frame in recorder.server_reads.lock().iter() {
        check_prefix(frame, "sim server read")
            .unwrap_or_else(|err| panic!("seed {}: {err}", schedule.faults.seed));
    }
    for frame in recorder.client_reads.lock().iter() {
        check_exact(frame, schedule.payload, "sim client read")
            .unwrap_or_else(|err| panic!("seed {}: {err}", schedule.faults.seed));
    }
    // INV-1 (sim half): books closed, world drained.
    assert_sim_drained(&net, &format!("seed {}", schedule.faults.seed));
}

async fn run_sim_scenario(net: SimNetwork, schedule: &Schedule, recorder: Arc<Recorder>) {
    let (server, server_addr, cert) =
        quic_sim::sim_server_endpoint(&net, Some(bounded_transport(IDLE_TIMEOUT)));

    // The accept loop runs as its own task; every decision happens at the
    // accept point inside it (the A1 shape).
    {
        let server = server.clone();
        let schedule = schedule.clone();
        let recorder = Arc::clone(&recorder);
        net.spawn(sim_server_loop(server, schedule, recorder));
    }

    let mut clients = Vec::new();
    let mut client_endpoints = Vec::new();
    for index in 0..schedule.connections {
        let client = quic_sim::sim_client_endpoint(&net, cert.clone());
        client_endpoints.push(client.clone());
        clients.push(sim_client_run(
            net.clone(),
            client,
            server_addr,
            index,
            schedule,
            Arc::clone(&recorder),
        ));
    }
    futures::future::join(futures::future::join_all(clients), async {
        // Ops fire at fixed virtual milestones while traffic runs.
        for op in &schedule.ops {
            net.sim_sleep(SIM_OP_MILESTONE).await;
            if let Op::IdleAdvanceMs(ms) = op {
                net.advance(Duration::from_millis(*ms));
            }
            net.sim_sleep(SIM_OP_MILESTONE).await;
        }
    })
    .await;

    // Deterministic close, twice: exercises the duplicate-shutdown path
    // (the ShutdownDup op only adds an earlier occurrence of the same).
    for endpoint in &client_endpoints {
        endpoint.close(0u32.into(), b"sim shutdown");
        endpoint.close(0u32.into(), b"sim shutdown");
    }
    server.close(0u32.into(), b"sim shutdown");
    server.close(0u32.into(), b"sim shutdown");
    net.sim_timeout(Duration::from_secs(10), server.wait_idle())
        .await;
}

async fn sim_server_loop(server: Endpoint, schedule: Schedule, recorder: Arc<Recorder>) {
    let mut retried: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    while let Some(incoming) = server.accept().await {
        let remote_port = incoming.remote_address().port();
        if schedule.use_retry && retried.insert(remote_port) {
            // Stateless retry on first contact; the token-carrying
            // retransmission arrives from the same remote and is accepted
            // on a later loop iteration.
            let _ = incoming.retry();
            continue;
        }
        let Ok(connection) = incoming.await else {
            recorder.fail();
            continue;
        };
        sim_server_connection(&connection, &schedule, &recorder).await;
    }
}

async fn sim_server_connection(
    connection: &quinn::Connection,
    schedule: &Schedule,
    recorder: &Recorder,
) {
    if schedule.reverse {
        // Server writes: send-path-biased leg.
        for stream in 0..schedule.streams_per_conn {
            let frame = framed_payload(0, stream, schedule.payload);
            let Ok(mut send) = connection.open_uni().await else {
                recorder.fail();
                return;
            };
            if send.write_all(&frame).await.is_err() {
                recorder.fail();
                return;
            }
            let _ = send.finish();
        }
        return;
    }
    for _ in 0..schedule.streams_per_conn {
        let Ok(mut incoming) = connection.accept_uni().await else {
            recorder.fail();
            return;
        };
        let cap = schedule.payload + 8 + STREAM_CAP_SLACK;
        let Ok(bytes) = incoming.read_to_end(cap).await else {
            recorder.fail();
            return;
        };
        recorder.server_reads.lock().push(bytes.clone());
        // Echo; a peer that dropped mid-transfer just stops hearing us.
        if let Ok(mut reply) = connection.open_uni().await {
            let _ = reply.write_all(&bytes).await;
            let _ = reply.finish();
        }
    }
}

async fn sim_client_run(
    net: SimNetwork,
    client: Endpoint,
    server_addr: std::net::SocketAddr,
    index: usize,
    schedule: &Schedule,
    recorder: Arc<Recorder>,
) {
    let drop_point = schedule.ops.iter().find_map(|op| match op {
        Op::DropClient { index: target, at } if target % schedule.connections.max(1) == index => {
            Some(*at)
        }
        _ => None,
    });
    let connecting = client
        .connect(server_addr, "localhost")
        .expect("connect setup");
    if drop_point == Some(DropPoint::MidHandshake) {
        drop(connecting);
        drop(client);
        recorder.fail();
        return;
    }
    let Ok(connection) = connecting.await else {
        recorder.fail();
        return;
    };
    if schedule.reverse {
        for _ in 0..schedule.streams_per_conn {
            let Ok(mut incoming) = connection.accept_uni().await else {
                recorder.fail();
                return;
            };
            let cap = schedule.payload + 8 + STREAM_CAP_SLACK;
            match incoming.read_to_end(cap).await {
                Ok(bytes) => {
                    if let Err(err) = check_exact(&bytes, schedule.payload, "sim reverse client") {
                        panic!("seed {}: {err}", schedule.faults.seed);
                    }
                    recorder.client_reads.lock().push(bytes);
                }
                Err(_) => {
                    recorder.fail();
                    return;
                }
            }
        }
    } else {
        for stream in 0..schedule.streams_per_conn {
            let frame = framed_payload(index, stream, schedule.payload);
            let Ok(mut send) = connection.open_uni().await else {
                recorder.fail();
                return;
            };
            if send.write_all(&frame).await.is_err() {
                recorder.fail();
                return;
            }
            if send.finish().is_err() {
                recorder.fail();
                return;
            }
            if drop_point == Some(DropPoint::MidTransfer) {
                drop(send);
                drop(connection);
                drop(client);
                return;
            }
        }
        for _ in 0..schedule.streams_per_conn {
            let Ok(mut reply) = connection.accept_uni().await else {
                recorder.fail();
                return;
            };
            let cap = schedule.payload + 8 + STREAM_CAP_SLACK;
            match reply.read_to_end(cap).await {
                Ok(bytes) => {
                    recorder.client_reads.lock().push(bytes);
                }
                Err(_) => {
                    recorder.fail();
                    return;
                }
            }
        }
    }
    connection.close(0u32.into(), b"done");
    let _ = net
        .sim_timeout(Duration::from_secs(2), connection.closed())
        .await;
}

// ─── Real-flavor scenario (current src/quic.rs adapter, loopback) ──────

const REAL_IDLE_TIMEOUT: Duration = Duration::from_millis(1200);
const REAL_BOUND: Duration = Duration::from_secs(10);

fn real_certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("rcgen");
    (
        cert.cert.into(),
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    )
}

fn real_server_config(
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
    crypto.alpn_protocols = vec![b"props/1".to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(crypto).expect("quic server crypto"),
    ));
    config.transport = bounded_transport(REAL_IDLE_TIMEOUT);
    config
}

fn real_client_config(cert: &CertificateDer<'static>) -> quinn::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone()).expect("valid certificate");
    let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3")
    .with_root_certificates(roots)
    .with_no_client_auth();
    crypto.alpn_protocols = vec![b"props/1".to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("quic crypto"),
    ));
    config.transport_config(bounded_transport(REAL_IDLE_TIMEOUT));
    config
}

/// One real-flavor leg: the Schedule over the current adapter on real
/// loopback. INV-3 is checked here; INV-1/INV-5/INV-6 by the quiescence
/// guard and the bounded repeated shutdown at the end.
fn run_real_leg(schedule: &Schedule) -> Result<(), String> {
    let _leg_lock = REAL_LEG_LOCK.lock();
    let guard = quiescent::QuiescenceGuard::start(Some(1));
    let runtime = Arc::clone(guard.runtime());
    let app = rust_miniss::Runtime::new();
    let quic_runtime: Arc<dyn quinn::Runtime> = Arc::new(
        rust_miniss::quic::MinissQuicRuntime::new(Arc::clone(&runtime)),
    );
    let (cert, key) = real_certs();
    let recorder = Arc::new(Recorder::default());

    let server = Endpoint::new(
        Default::default(),
        Some(real_server_config(&cert, &key)),
        std::net::UdpSocket::bind("127.0.0.1:0").expect("bind server"),
        Arc::clone(&quic_runtime),
    )
    .expect("server endpoint");
    let server_addr = server.local_addr().expect("server addr");
    let mut client = Endpoint::new(
        Default::default(),
        None,
        std::net::UdpSocket::bind("127.0.0.1:0").expect("bind client"),
        quic_runtime,
    )
    .expect("client endpoint");
    client.set_default_client_config(real_client_config(&cert));

    // Server side: one accept loop on the endpoint's shard, connections
    // handled sequentially inside the loop (every decision at the accept
    // point; no shared state beyond the loop's locals).
    let (server_done_tx, server_done_rx) = futures::channel::oneshot::channel::<()>();
    runtime
        .spawn_on(0, {
            let server = server.clone();
            let schedule = schedule.clone();
            let recorder = Arc::clone(&recorder);
            async move {
                let mut retried: std::collections::BTreeSet<u16> =
                    std::collections::BTreeSet::new();
                while let Ok(Some(incoming)) =
                    rust_miniss::timer::timeout(REAL_BOUND, server.accept()).await
                {
                    let remote_port = incoming.remote_address().port();
                    if schedule.use_retry && retried.insert(remote_port) {
                        let _ = incoming.retry();
                        continue;
                    }
                    let Ok(connection) = incoming.await else {
                        recorder.fail();
                        continue;
                    };
                    real_server_connection(&connection, &schedule, &recorder).await;
                }
                let _ = server_done_tx.send(());
            }
        })
        .expect("admit server accept loop");

    // Client side: connections run concurrently inside the block_on thread
    // (the tests/quinn_h3.rs shape).
    app.block_on(async {
        let mut clients = Vec::new();
        for index in 0..schedule.connections {
            let recorder = Arc::clone(&recorder);
            let client = client.clone();
            let schedule = schedule.clone();
            let drop_point = schedule.ops.iter().find_map(|op| match op {
                Op::DropClient { index: target, at }
                    if target % schedule.connections.max(1) == index =>
                {
                    Some(*at)
                }
                _ => None,
            });
            clients.push(async move {
                let connecting = client
                    .connect(server_addr, "localhost")
                    .expect("connect setup");
                if drop_point == Some(DropPoint::MidHandshake) {
                    drop(connecting);
                    drop(client);
                    recorder.fail();
                    return;
                }
                let connection = match rust_miniss::timer::timeout(REAL_BOUND, connecting).await {
                    Ok(Ok(connection)) => connection,
                    _ => {
                        recorder.fail();
                        return;
                    }
                };
                if schedule.reverse {
                    for _ in 0..schedule.streams_per_conn {
                        match rust_miniss::timer::timeout(REAL_BOUND, connection.accept_uni()).await
                        {
                            Ok(Ok(mut incoming)) => {
                                let cap = schedule.payload + 8 + STREAM_CAP_SLACK;
                                match incoming.read_to_end(cap).await {
                                    Ok(bytes) => {
                                        if let Err(err) = check_exact(
                                            &bytes,
                                            schedule.payload,
                                            "real reverse client",
                                        ) {
                                            panic!("{err}");
                                        }
                                        recorder.client_reads.lock().push(bytes);
                                    }
                                    Err(_) => {
                                        recorder.fail();
                                        return;
                                    }
                                }
                            }
                            _ => {
                                recorder.fail();
                                return;
                            }
                        }
                    }
                } else {
                    for stream in 0..schedule.streams_per_conn {
                        let frame = framed_payload(index, stream, schedule.payload);
                        let Ok(Ok(mut send)) =
                            rust_miniss::timer::timeout(REAL_BOUND, connection.open_uni()).await
                        else {
                            recorder.fail();
                            return;
                        };
                        if rust_miniss::timer::timeout(REAL_BOUND, send.write_all(&frame))
                            .await
                            .is_err()
                            || send.finish().is_err()
                        {
                            recorder.fail();
                            return;
                        }
                        if drop_point == Some(DropPoint::MidTransfer) {
                            drop(send);
                            drop(connection);
                            return;
                        }
                    }
                    for _ in 0..schedule.streams_per_conn {
                        match rust_miniss::timer::timeout(REAL_BOUND, connection.accept_uni()).await
                        {
                            Ok(Ok(mut reply)) => {
                                let cap = schedule.payload + 8 + STREAM_CAP_SLACK;
                                match reply.read_to_end(cap).await {
                                    Ok(bytes) => {
                                        recorder.client_reads.lock().push(bytes);
                                    }
                                    Err(_) => {
                                        recorder.fail();
                                        return;
                                    }
                                }
                            }
                            _ => {
                                recorder.fail();
                                return;
                            }
                        }
                    }
                }
                connection.close(0u32.into(), b"done");
            });
        }
        let _ = rust_miniss::timer::timeout(REAL_BOUND, futures::future::join_all(clients)).await;

        // Close both endpoints (twice), then wait for the accept loop to
        // observe the closure — all inside the app runtime so the miniss
        // timer works.
        server.close(0u32.into(), b"props done");
        server.close(0u32.into(), b"props done");
        client.close(0u32.into(), b"props done");
        client.close(0u32.into(), b"props done");
        let _ = rust_miniss::timer::timeout(REAL_BOUND, server_done_rx).await;
    });
    // Release the endpoint handles (each owns a UDP fd) before the
    // quiescence oracle measures the process fd balance.
    drop(client);
    drop(server);

    // INV-3 verdicts.
    for frame in recorder.server_reads.lock().iter() {
        check_prefix(frame, "real server read")?;
    }
    for frame in recorder.client_reads.lock().iter() {
        check_exact(frame, schedule.payload, "real client read")?;
    }

    // INV-6: bounded drain, including a repeated shutdown injection; the
    // guard then applies INV-1 + INV-5 (snapshot resources balanced+empty,
    // fd balance) on the fully shut-down runtime.
    let io_runtime = Arc::clone(guard.runtime());
    io_runtime
        .shutdown_with_timeout(Duration::from_secs(3))
        .map_err(|err| format!("INV-6: first bounded shutdown failed: {err}"))?;
    io_runtime
        .shutdown_with_timeout(Duration::from_secs(3))
        .map_err(|err| format!("INV-6: repeated shutdown failed: {err}"))?;
    guard.finish_quiescent("quic props real leg");
    Ok(())
}

async fn real_server_connection(
    connection: &quinn::Connection,
    schedule: &Schedule,
    recorder: &Recorder,
) {
    if schedule.reverse {
        for stream in 0..schedule.streams_per_conn {
            let frame = framed_payload(0, stream, schedule.payload);
            let Ok(mut send) = connection.open_uni().await else {
                recorder.fail();
                return;
            };
            if send.write_all(&frame).await.is_err() {
                recorder.fail();
                return;
            }
            let _ = send.finish();
        }
        return;
    }
    for _ in 0..schedule.streams_per_conn {
        let Ok(mut incoming) = connection.accept_uni().await else {
            recorder.fail();
            return;
        };
        let cap = schedule.payload + 8 + STREAM_CAP_SLACK;
        let Ok(Ok(bytes)) =
            rust_miniss::timer::timeout(REAL_BOUND, incoming.read_to_end(cap)).await
        else {
            recorder.fail();
            return;
        };
        recorder.server_reads.lock().push(bytes.clone());
        if let Ok(mut reply) = connection.open_uni().await {
            let _ = reply.write_all(&bytes).await;
            let _ = reply.finish();
        }
    }
}

// ─── PROBE (methodology §4.2): environment validity per leg ────────────

/// PROBE: before differential legs run, prove the backend really consumes
/// completions on the QUIC path (D22 vaccine): one minimal handshake plus
/// one stream byte must complete inside a bounded window.
#[test]
fn probe_backend_consumes_quic_completions() {
    let schedule = Schedule {
        connections: 1,
        streams_per_conn: 1,
        payload: 1,
        use_retry: false,
        faults: SimFaultSchedule::default(),
        reverse: false,
        ops: Vec::new(),
    };
    run_real_leg(&schedule).expect("PROBE: backend did not consume completions");
    let backend = if cfg!(io_backend = "io_uring") {
        "io_uring"
    } else if cfg!(io_backend = "epoll") {
        "epoll"
    } else if cfg!(io_backend = "kqueue") {
        "kqueue"
    } else {
        "unknown"
    };
    println!("PROBE ok: backend={backend} consumed QUIC completions");
}

// ─── DIFF-1 (methodology §4.2): same Schedule, all legs, same verdict ──

/// DIFF-1: the same Schedule runs on every leg this host compiles (macOS:
/// kqueue; Linux CI: io_uring / epoll run the same property through the
/// same code) and in both transfer directions — recv-path-biased and
/// send-path-biased, the twin implementations in src/quic.rs; all
/// invariants must agree across legs.
#[test]
fn diff1_same_schedule_same_verdict_across_legs() {
    let schedule = Schedule {
        connections: 3,
        streams_per_conn: 2,
        payload: 1024,
        use_retry: false,
        faults: SimFaultSchedule::default(),
        reverse: false,
        ops: vec![Op::DropClient {
            index: 2,
            at: DropPoint::MidTransfer,
        }],
    };
    let mut forward = schedule.clone();
    forward.reverse = false;
    let mut reverse_leg = schedule.clone();
    reverse_leg.reverse = true;

    let forward_verdict = run_real_leg(&forward);
    let reverse_verdict = run_real_leg(&reverse_leg);
    assert_eq!(
        forward_verdict.is_ok(),
        reverse_verdict.is_ok(),
        "DIFF-1: legs disagree: forward={forward_verdict:?} reverse={reverse_verdict:?}"
    );
    forward_verdict.expect("forward leg");
    reverse_verdict.expect("reverse leg");
}

// ─── Property tests ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Sim-flavor: any Schedule completes its transfers intact under the
    /// seeded fault schedule and drains to quiescence (INV-1 + INV-3).
    #[test]
    fn sim_flavor_schedule_integrity_and_drain(schedule in arb_schedule()) {
        run_sim(&schedule);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]

    /// Real-flavor: the same Schedule dimensions on the current adapter
    /// over loopback; INV-1 + INV-3 + INV-5 + INV-6 per case.
    #[test]
    fn real_flavor_schedule_integrity_and_quiescence(schedule in arb_schedule()) {
        let mut real = schedule;
        // Net faults are sim-side; loopback is reliable. Retry stays a
        // policy dimension; drops and op interleave carry over.
        real.faults = SimFaultSchedule::default();
        run_real_leg(&real).unwrap_or_else(|err| panic!("real leg failed: {err}"));
    }
}
