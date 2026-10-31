//! T4 control: the SAME per-connection churn quantities as T3
//! (`benches/quic_conn_lifecycle.rs`), measured on compio-quic — the only
//! production-viable io_uring-native QUIC stack (compio ecosystem, reuses
//! quinn-proto) — as the runtime comparison the owner asked for before
//! deciding on io_uring ("io_uring 的运行时我要看到性能差距再做决定").
//!
//! Measurement only — NO pass-line assert (the go/no-go line stays with
//! `quic_conn_lifecycle.rs`), and NO spawn baseline (compio has no TaskArena
//! analogue to compare against).
//!
//! Phases (mirrored from T3 exactly — same counts, same timeout wraps, same
//! measurement definitions, same loopback topology):
//! 1. probe handshake — one bounded connect proving completions are consumed;
//! 2. sequential churn — 100 × (connect → TLS 1.3 handshake → close → drop),
//!    reported as the mean per-connection µs;
//! 3. concurrent churn — c8 and c32 logical widths over 200 connections,
//!    reported as total elapsed / 200 µs per connection.
//!
//! TLS mirrors T3 verbatim: rcgen self-signed "localhost" cert, rustls ring
//! provider, TLS 1.3 only, ALPN "t3/1", 30 s max idle timeout, no client
//! auth, client roots = the server cert. Every connect is wrapped in a 5 s
//! timeout like T3. Server side accepts and drops (implicit close) — the
//! full server-side lifecycle cost.
//!
//! Runtime model. `compio_runtime::Runtime::new().block_on(run_bench())` is
//! the closest compio analogue of one miniss shard and of the tokio
//! control's `current_thread` flavor: a single thread runs the thread-local
//! executor and the I/O driver, so per-connection work never migrates. On
//! Linux compio-driver's default (and only compiled) backend is io_uring, so
//! on orb this measures an io_uring-native stack against miniss's io_uring —
//! that is the real comparison. On macOS compio falls back to the `polling`
//! driver (kqueue); a macOS run is therefore NOT io_uring and is labelled as
//! directional only.
//!
//! Fairness caveats — this comparison is "cost of a full QUIC handshake
//! cycle on an io_uring-native runtime", NOT API equivalence. Known
//! API-shape deviations from the T3/quin stack, each of which can shift the
//! number either way:
//! - Buffer models differ. compio is an owned-`IoBuf`/`BufResult` stack;
//!   quinn uses its own buffer management. Per-datagram copies/allocation
//!   shapes are not identical.
//! - Connection API differs. compio-quic's `Endpoint` is `!Send`
//!   (thread-local runtime): server accept is a `wait_incoming()` loop on a
//!   locally spawned task, while T3 uses miniss's `QuicAcceptor`, which adds
//!   an admission gate (`QuicLimits::max_connections`) compio does not have.
//!   Both accept loops drop every churn connection implicitly.
//! - Endpoint driver placement differs. quinn's endpoint driver runs as a
//!   spawned task on the miniss executor via `MinissQuicRuntime`;
//!   compio-quic spawns its endpoint driver on the compio runtime that owns
//!   the socket. Both are single-threaded here, but the driver/task
//!   interaction is each stack's own.
//! - Timer wheels differ (miniss 1 ms wheel vs compio's driver-integrated
//!   timer); the 5 s connect wraps and 100 ms inter-phase sleeps are the
//!   only timed waits in the measured path, and they are idle deadlines, so
//!   the quantization difference does not touch the measured window.
//! - The churn loop bodies below are line-for-line the T3 concurrent shape
//!   (`futures::future::join_all` of `width` async blocks inside ONE task),
//!   so the concurrency semantics of the client side are identical.

use compio_quic::{ClientBuilder, Endpoint, ServerBuilder, TransportConfig};
use futures_util::future::join_all;
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHURN_CONNECTIONS: usize = 100;
const CONCURRENT_CONNECTIONS: usize = 200;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// compio I/O driver label. compio-driver compiles io-uring (its default
/// feature) on Linux and only `polling` on every other unix, so the label is
/// a compile-time fact, verified per-build via `cargo tree -e features`.
fn compio_backend() -> &'static str {
    if cfg!(target_os = "linux") {
        "io_uring"
    } else if cfg!(target_os = "macos") {
        "polling/kqueue (NOT io_uring — directional only)"
    } else {
        "polling (NOT io_uring)"
    }
}

struct BenchReport {
    sequential_churn_us: f64,
    concurrent_c8_us: f64,
    concurrent_c32_us: f64,
    idle_cpu_fraction: f64,
}

fn main() {
    let backend = compio_backend();

    // PROBE (T3 D44): the process must be idle over a 200 ms window, measured
    // before the runtime exists — same placement as both T3 benches.
    let idle_cpu_fraction_before = idle_cpu_probe();

    let runtime = compio_runtime::Runtime::new().expect("compio runtime");
    let report = runtime.block_on(run_bench());

    println!("T4 compio_quic_churn (compio backend={backend}, compio-quic 0.8/quinn-proto 0.11)");
    println!(
        "  PROBE pre-bench idle cpu fraction: {:.4}",
        idle_cpu_fraction_before
    );
    println!(
        "  sequential churn:          {:>9.1} µs/conn",
        report.sequential_churn_us
    );
    println!(
        "  concurrent churn c8:       {:>9.1} µs/conn",
        report.concurrent_c8_us
    );
    println!(
        "  concurrent churn c32:      {:>9.1} µs/conn",
        report.concurrent_c32_us
    );
    println!(
        "  PROBE post-bench idle cpu fraction: {:.4}",
        report.idle_cpu_fraction
    );
    // No pass-line assert: this bench measures, the miniss bench decides.
}

async fn run_bench() -> BenchReport {
    let (cert, key) = certs();

    let server = Endpoint::server("127.0.0.1:0", server_config(&cert, &key))
        .await
        .expect("server endpoint");
    let addr: SocketAddr = server.local_addr().expect("server addr");
    let _ = &server; // handle kept alive for the endpoint driver

    // Server accept loop: churn conns are accepted and dropped (implicit
    // close), which is the full server-side lifecycle cost. compio-quic has
    // no QuicAcceptor analogue — the endpoint accepts directly (see the
    // fairness caveats in the header). A churn client closes immediately
    // after its handshake completes, so `incoming.await` can lose the race
    // with the peer's CONNECTION_CLOSE; T3's miniss acceptor treats that as
    // `handshake_failures += 1` and keeps looping (src/quic/acceptor.rs),
    // and this loop mirrors that tolerance instead of panicking. The
    // JoinHandle is detached: compio cancels a spawned task when its handle
    // drops, and this loop must live for the whole bench.
    let acceptor = server.clone();
    compio_runtime::spawn(async move {
        while let Some(incoming) = acceptor.wait_incoming().await {
            // Ok: established, then dropped (implicit close). Err: peer
            // mid-handshake close — counted as T3's acceptor counts it.
            let _ = incoming.await;
        }
    })
    .detach();

    let mut client = Endpoint::client("127.0.0.1:0")
        .await
        .expect("client endpoint");
    let mut client_cfg =
        ClientBuilder::new_with_rustls_client_config(rustls_client_crypto(&cert)).build();
    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("idle")));
    client_cfg.transport_config(Arc::new(transport));
    client.default_client_config = Some(client_cfg);

    // PROBE (T3 D22): completions are consumed.
    let connection = compio_runtime::time::timeout(
        CONNECT_TIMEOUT,
        client.connect(addr, "localhost", None).expect("connect"),
    )
    .await
    .expect("probe handshake bounded")
    .expect("probe handshake ok");
    connection.close(0u32.into(), b"probe");
    compio_runtime::time::sleep(Duration::from_millis(50)).await;

    // 1. Sequential handshake churn (T3 phase 2, spawn baseline skipped).
    let mut sequential_samples = Vec::with_capacity(CHURN_CONNECTIONS);
    for _ in 0..CHURN_CONNECTIONS {
        let start = Instant::now();
        let connection = compio_runtime::time::timeout(
            CONNECT_TIMEOUT,
            client.connect(addr, "localhost", None).expect("connect"),
        )
        .await
        .expect("churn handshake bounded")
        .expect("churn handshake ok");
        connection.close(0u32.into(), b"churn");
        drop(connection);
        sequential_samples.push(start.elapsed().as_secs_f64() * 1e6);
    }
    let sequential_churn_us =
        sequential_samples.iter().sum::<f64>() / sequential_samples.len() as f64;
    compio_runtime::time::sleep(Duration::from_millis(100)).await;

    // 2. Concurrent churn c8 and c32 (T3 phase 3): `width` async blocks
    // joined inside one task, 200 connections total, per-conn = total/200.
    let concurrent = |client: &Endpoint, addr: SocketAddr, width: usize| {
        let client = client.clone();
        async move {
            let per_task = CONCURRENT_CONNECTIONS / width;
            let start = Instant::now();
            let mut tasks = Vec::with_capacity(width);
            for _ in 0..width {
                let client = client.clone();
                tasks.push(async move {
                    for _ in 0..per_task {
                        let connection = compio_runtime::time::timeout(
                            CONNECT_TIMEOUT,
                            client.connect(addr, "localhost", None).expect("connect"),
                        )
                        .await
                        .expect("concurrent churn handshake bounded")
                        .expect("concurrent churn handshake ok");
                        connection.close(0u32.into(), b"churn");
                        drop(connection);
                    }
                });
            }
            join_all(tasks).await;
            start.elapsed().as_secs_f64() * 1e6 / CONCURRENT_CONNECTIONS as f64
        }
    };
    let concurrent_c8_us = concurrent(&client, addr, 8).await;
    compio_runtime::time::sleep(Duration::from_millis(100)).await;
    let concurrent_c32_us = concurrent(&client, addr, 32).await;
    compio_runtime::time::sleep(Duration::from_millis(100)).await;

    BenchReport {
        sequential_churn_us,
        concurrent_c8_us,
        concurrent_c32_us,
        idle_cpu_fraction: idle_cpu_probe(),
    }
}

/// Idle probe: user+system CPU consumed by this process over 200 ms.
/// Verbatim from the T3 benches.
fn idle_cpu_probe() -> f64 {
    let before = getrusage_self();
    std::thread::sleep(Duration::from_millis(200));
    let after = getrusage_self();
    let busy_micros =
        (after.user_us + after.system_us).saturating_sub(before.user_us + before.system_us);
    busy_micros as f64 / 200_000.0
}

struct RusageSelf {
    user_us: u64,
    system_us: u64,
}

fn getrusage_self() -> RusageSelf {
    #[repr(C)]
    struct Timeval {
        sec: i64,
        usec: i64,
    }
    #[repr(C)]
    struct RUsage {
        ru_utime: Timeval,
        ru_stime: Timeval,
        // The rest of the struct is not needed here; zero it.
        _rest: [u64; 14],
    }
    let mut ru: RUsage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid RUsage layout for RUSAGE_SELF (macOS/Linux
    // layouts agree on the first two timevals), and libc::getrusage only
    // writes through the pointer.
    let ret = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru as *mut RUsage as *mut _) };
    debug_assert_eq!(ret, 0);
    RusageSelf {
        user_us: (ru.ru_utime.sec as u64) * 1_000_000 + ru.ru_utime.usec as u64,
        system_us: (ru.ru_stime.sec as u64) * 1_000_000 + ru.ru_stime.usec as u64,
    }
}

fn certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = generate_simple_self_signed(vec!["localhost".into()]).expect("rcgen");
    (
        cert.cert.into(),
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    )
}

/// T3's server TLS config, ending in a compio-quic `ServerConfig` instead of
/// a quinn one. The rustls half is identical: ring provider, TLS 1.3 only,
/// ALPN "t3/1", no client auth, single cert.
fn server_config(
    cert: &CertificateDer<'static>,
    key: &PrivateKeyDer<'static>,
) -> compio_quic::ServerConfig {
    let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.clone_key())
    .expect("server cert");
    crypto.alpn_protocols = vec![b"t3/1".to_vec()];
    let mut config = ServerBuilder::new_with_rustls_server_config(crypto).build();
    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("idle")));
    config.transport = Arc::new(transport);
    config
}

/// T3's client TLS trust shape (rustls half): ring provider, TLS 1.3 only,
/// root store pinned to the server's self-signed cert, ALPN "t3/1".
fn rustls_client_crypto(cert: &CertificateDer<'static>) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone()).expect("valid certificate");
    let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3")
    .with_root_certificates(roots)
    .with_no_client_auth();
    crypto.alpn_protocols = vec![b"t3/1".to_vec()];
    crypto
}
