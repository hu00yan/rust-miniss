//! T3: per-connection lifecycle microbenchmark (methodology plan §步骤零;
//! owner execution order step 7).
//!
//! Measures, on one shard against a real loopback endpoint:
//! 1. spawn baseline — `spawn_local` of an immediately-completing task;
//! 2. sequential handshake churn — connect, handshake, close, drop;
//! 3. concurrent handshake churn at c8 and c32 — the go/no-go quantity;
//! 4. cross-task wake — connection A's traffic wakes connection B's task.
//!
//! PROBE preconditions (D44/D45): before any measurement the bench proves
//! the backend consumes completions (a handshake completes) and that the
//! process is idle (no busy loop) over a 200 ms window.
//!
//! Pass line (plan §4 步骤零): concurrent churn per-connection cost
//! < ~100 µs, without collapsing to 发现 C's ~300 µs serialized
//! signature. The bench self-asserts and exits non-zero on breach; the
//! measured number is recorded verbatim in the task report — never
//! massaged.

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rcgen::generate_simple_self_signed;
use rust_miniss::quic::{QuicAcceptor, QuicLimits};
use rust_miniss::MultiCoreRuntime;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHURN_CONNECTIONS: usize = 100;
const CONCURRENT_CONNECTIONS: usize = 200;
const WAKE_ITERATIONS: usize = 500;
const PASS_LINE_MICROS: f64 = 100.0;

struct BenchReport {
    spawn_baseline_us: f64,
    sequential_churn_us: f64,
    concurrent_c8_us: f64,
    concurrent_c32_us: f64,
    wake_p50_us: f64,
    wake_p99_us: f64,
    idle_cpu_fraction: f64,
}

fn main() {
    let backend = if cfg!(io_backend = "io_uring") {
        "io_uring"
    } else if cfg!(io_backend = "epoll") {
        "epoll"
    } else if cfg!(io_backend = "kqueue") {
        "kqueue"
    } else {
        "unknown"
    };

    // PROBE (D44): the process must be idle over a 200 ms window.
    let idle_cpu_fraction_before = idle_cpu_probe();

    let runtime = MultiCoreRuntime::new(Some(1)).expect("one-shard runtime");
    let (tx, rx) = std::sync::mpsc::channel();
    let runtime_for_bench = Arc::clone(&runtime);
    runtime
        .spawn_on(0, {
            let tx = tx.clone();
            async move {
                let report = run_bench(runtime_for_bench).await;
                let _ = tx.send(report);
            }
        })
        .expect("admit bench task");

    let report = rx
        .recv_timeout(Duration::from_secs(120))
        .expect("bench completed");

    println!("T3 quic_conn_lifecycle (backend={backend})");
    println!(
        "  PROBE pre-bench idle cpu fraction: {:.4}",
        idle_cpu_fraction_before
    );
    println!(
        "  spawn baseline:            {:>9.1} µs/task",
        report.spawn_baseline_us
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
        "  cross-task wake p50/p99:   {:>9.1} / {:.1} µs",
        report.wake_p50_us, report.wake_p99_us
    );

    // PROBE (D45): the measurement environment must be quiet.
    assert!(
        report.idle_cpu_fraction < 0.25,
        "PROBE: process was busy during the idle window ({:.4}); numbers are not trustworthy",
        report.idle_cpu_fraction
    );
    // Pass line (plan §步骤零): concurrent churn per connection under
    // ~100 µs; the ~300 µs 发现 C signature is the freeze line.
    for (label, value) in [
        ("c8", report.concurrent_c8_us),
        ("c32", report.concurrent_c32_us),
    ] {
        if value >= PASS_LINE_MICROS {
            eprintln!(
                "T3 FAIL: concurrent churn {label} = {value:.1} µs/conn >= {PASS_LINE_MICROS} µs pass line"
            );
            std::process::exit(2);
        }
    }
    println!("  PASS: concurrent churn per-connection < {PASS_LINE_MICROS} µs pass line");
    let _ = backend;
}

/// Idle probe: user+system CPU consumed by this process over 200 ms.
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
    crypto.alpn_protocols = vec![b"t3/1".to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(crypto).expect("quic server crypto"),
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("idle")));
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
    crypto.alpn_protocols = vec![b"t3/1".to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("quic crypto"),
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("idle")));
    config.transport_config(Arc::new(transport));
    config
}

/// PROBE (D22): prove the backend consumes completions before measuring.
async fn probe_handshake(client: &mut quinn::Endpoint, addr: std::net::SocketAddr) {
    let connection = rust_miniss::timer::timeout(
        Duration::from_secs(5),
        client.connect(addr, "localhost").expect("connect"),
    )
    .await
    .expect("probe handshake bounded")
    .expect("probe handshake ok");
    connection.close(0u32.into(), b"probe");
}

async fn run_bench(runtime: Arc<MultiCoreRuntime>) -> BenchReport {
    let (cert, key) = certs();

    let server = quinn::Endpoint::new(
        Default::default(),
        Some(server_config(&cert, &key)),
        std::net::UdpSocket::bind("127.0.0.1:0").expect("bind server"),
        Arc::new(rust_miniss::quic::MinissQuicRuntime::new(Arc::clone(
            &runtime,
        ))),
    )
    .expect("server endpoint");
    let addr = server.local_addr().expect("server addr");
    let _ = &server; // handle kept alive for the endpoint driver

    // Server accept loop: churn conns are accepted and dropped (implicit
    // close), which is the full server-side lifecycle cost.
    let server_for_acceptor = server.clone();
    runtime
        .spawn_local(async move {
            let mut acceptor = QuicAcceptor::new(
                server_for_acceptor,
                Arc::new(QuicLimits {
                    max_connections: 4096,
                    use_retry: false,
                }),
            );
            while acceptor.next_connection().await.is_some() {}
        })
        .expect("admit acceptor loop");

    let mut client = quinn::Endpoint::new(
        Default::default(),
        None,
        std::net::UdpSocket::bind("127.0.0.1:0").expect("bind client"),
        Arc::new(rust_miniss::quic::MinissQuicRuntime::new(Arc::clone(
            &runtime,
        ))),
    )
    .expect("client endpoint");
    client.set_default_client_config(client_config(&cert));

    // PROBE (D22): completions are consumed.
    probe_handshake(&mut client, addr).await;
    rust_miniss::timer::sleep(Duration::from_millis(50)).await;

    // 1. Spawn baseline: spawn + poll + drop of an immediately-completing
    // task, measured from spawn start until every task ran.
    const SPAWNS: usize = 20_000;
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let start = Instant::now();
    for _ in 0..SPAWNS {
        let completed = Arc::clone(&completed);
        runtime
            .spawn_local(async move {
                completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
            .expect("spawn baseline task");
    }
    // Await completion: the spawned tasks run on this same shard, so the
    // wait must yield to the executor rather than spin the thread.
    while completed.load(std::sync::atomic::Ordering::Relaxed) < SPAWNS {
        rust_miniss::timer::sleep(Duration::from_micros(200)).await;
    }
    let spawn_baseline_us = start.elapsed().as_secs_f64() * 1e6 / SPAWNS as f64;

    // 2. Sequential handshake churn.
    let mut sequential_samples = Vec::with_capacity(CHURN_CONNECTIONS);
    for _ in 0..CHURN_CONNECTIONS {
        let start = Instant::now();
        let connection = rust_miniss::timer::timeout(
            Duration::from_secs(5),
            client.connect(addr, "localhost").expect("connect"),
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
    rust_miniss::timer::sleep(Duration::from_millis(100)).await;

    // 3. Concurrent churn c8 and c32.
    let concurrent = |client: &quinn::Endpoint, width: usize| {
        let client = client.clone();
        async move {
            let per_task = CONCURRENT_CONNECTIONS / width;
            let start = Instant::now();
            let mut tasks = Vec::with_capacity(width);
            for _ in 0..width {
                let client = client.clone();
                tasks.push(async move {
                    for _ in 0..per_task {
                        let connection = rust_miniss::timer::timeout(
                            Duration::from_secs(5),
                            client.connect(addr, "localhost").expect("connect"),
                        )
                        .await
                        .expect("concurrent churn handshake bounded")
                        .expect("concurrent churn handshake ok");
                        connection.close(0u32.into(), b"churn");
                        drop(connection);
                    }
                });
            }
            futures::future::join_all(tasks).await;
            start.elapsed().as_secs_f64() * 1e6 / CONCURRENT_CONNECTIONS as f64
        }
    };
    let concurrent_c8_us = concurrent(&client, 8).await;
    rust_miniss::timer::sleep(Duration::from_millis(100)).await;
    let concurrent_c32_us = concurrent(&client, 32).await;
    rust_miniss::timer::sleep(Duration::from_millis(100)).await;

    // 4. Cross-task wake: A's traffic wakes B's task. A dedicated endpoint
    // serves the wake phase (the churn acceptor drops every churn
    // connection by design, which would close these two).
    let server2 = quinn::Endpoint::new(
        Default::default(),
        Some(server_config(&cert, &key)),
        std::net::UdpSocket::bind("127.0.0.1:0").expect("bind wake server"),
        Arc::new(rust_miniss::quic::MinissQuicRuntime::new(Arc::clone(
            &runtime,
        ))),
    )
    .expect("wake server endpoint");
    let wake_addr = server2.local_addr().expect("wake server addr");
    let read_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let wake_reads = Arc::clone(&read_count);
    runtime
        .spawn_local(async move {
            let mut acceptor = QuicAcceptor::new(
                server2,
                Arc::new(QuicLimits {
                    max_connections: 16,
                    use_retry: false,
                }),
            );
            let conn_a = acceptor.next_connection().await.expect("wake conn A");
            let conn_b = acceptor.next_connection().await.expect("wake conn B");
            // Server side: relay one byte from A's uni stream to a
            // server-opened uni stream on B, waking B's driver per byte.
            // The reader paces the writer, so this task stays alive until
            // every byte is consumed and B's stream finishes cleanly.
            let mut incoming = conn_a.accept_uni().await.expect("relay accept");
            let mut out = conn_b.open_uni().await.expect("relay open");
            for _ in 0..WAKE_ITERATIONS {
                let mut byte = [0u8; 1];
                incoming.read_exact(&mut byte).await.expect("relay read");
                out.write_all(&byte).await.expect("relay write");
            }
            let _ = out.finish();
            // Outlive the reader: an early close would reset unread data.
            while wake_reads.load(std::sync::atomic::Ordering::Relaxed) < WAKE_ITERATIONS {
                rust_miniss::timer::sleep(Duration::from_micros(200)).await;
            }
        })
        .expect("admit relay task");

    let conn_a = rust_miniss::timer::timeout(
        Duration::from_secs(5),
        client.connect(wake_addr, "localhost").expect("connect"),
    )
    .await
    .expect("wake handshake A bounded")
    .expect("wake handshake A ok");
    let conn_b = rust_miniss::timer::timeout(
        Duration::from_secs(5),
        client.connect(wake_addr, "localhost").expect("connect"),
    )
    .await
    .expect("wake handshake B bounded")
    .expect("wake handshake B ok");

    let wake_samples = Arc::new(std::sync::Mutex::new(Vec::with_capacity(WAKE_ITERATIONS)));
    let reader_count = Arc::clone(&read_count);
    let reader_wake = Arc::clone(&wake_samples);
    let reader = async move {
        // The relay opens its server-to-B stream before the first byte;
        // B's driver is woken per relayed byte.
        let mut reply = conn_b.accept_uni().await.expect("client B accept");
        for _ in 0..WAKE_ITERATIONS {
            let start = Instant::now();
            let mut byte = [0u8; 1];
            reply.read_exact(&mut byte).await.expect("client B read");
            reader_wake
                .lock()
                .unwrap()
                .push(start.elapsed().as_micros() as u64);
            reader_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    };
    futures::future::join(reader, async move {
        let mut client_a_stream = conn_a.open_uni().await.expect("client A open");
        for i in 0..WAKE_ITERATIONS {
            client_a_stream
                .write_all(b"x")
                .await
                .expect("client A write");
            // Pace on the reader so the relay cannot complete and close
            // the connections ahead of B's reads.
            while read_count.load(std::sync::atomic::Ordering::Relaxed) <= i {
                rust_miniss::timer::sleep(Duration::from_micros(200)).await;
            }
        }
    })
    .await;
    let mut wake_samples = Arc::try_unwrap(wake_samples)
        .expect("reader completed")
        .into_inner()
        .unwrap();
    wake_samples.sort_unstable();
    let wake_p50_us = wake_samples[wake_samples.len() / 2] as f64;
    let wake_p99_us = wake_samples[(wake_samples.len() as f64 * 0.99) as usize] as f64;

    BenchReport {
        spawn_baseline_us,
        sequential_churn_us,
        concurrent_c8_us,
        concurrent_c32_us,
        wake_p50_us,
        wake_p99_us,
        idle_cpu_fraction: idle_cpu_probe(),
    }
}
