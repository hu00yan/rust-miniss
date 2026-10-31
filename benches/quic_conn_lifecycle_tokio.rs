//! T3 control: the SAME per-connection lifecycle quantities measured on
//! tokio + quinn, as the calibration reference for the miniss numbers.
//!
//! Measurement only — no pass-line assert here; the go/no-go line is the
//! owner's to re-set from this control, and `benches/quic_conn_lifecycle.rs`
//! stays the enforcing bench.
//!
//! Runtime model. `#[tokio::main(flavor = "current_thread")]` is the closest
//! tokio analogue of one miniss shard: a single thread runs every task and
//! one I/O driver, so per-connection work never migrates and there is no
//! work-stealing to confound the comparison. It is NOT identical to miniss:
//! tokio's driver and timer wheel are part of the runtime (not a spawned
//! endpoint task on a user executor), tokio's timer wheel is 1 ms-granular
//! (like miniss's, which matters for the wake phase's sub-ms pacing), and
//! the bench body here is the runtime's root task rather than a
//! `spawn_local` task. current_thread + quinn's `TokioRuntime` is the fair
//! per-core comparison; multi-threaded tokio would measure a different
//! scheduling model, not a different QUIC stack.
//!
//! Reads the same diagnostic knobs as the miniss bench so a profiling
//! driver treats both identically:
//!   MINISS_T3_ONLY=spawn|churn|wake|udp   isolate one phase (skip assert)
//!   MINISS_T3_SCALE=<n>                   multiply iteration counts
//!   MINISS_T3_WAKE_PACE_US=<µs>           wake writer pacing (default 200)

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SPAWN_BASE_TASKS: usize = 20_000;
const CHURN_CONNECTIONS: usize = 100;
const CONCURRENT_CONNECTIONS: usize = 200;
const WAKE_ITERATIONS: usize = 500;
const UDP_FLOOR_ITERATIONS: usize = 2_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    All,
    Spawn,
    Churn,
    Wake,
    Udp,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::All => "all",
            Phase::Spawn => "spawn",
            Phase::Churn => "churn",
            Phase::Wake => "wake",
            Phase::Udp => "udp",
        }
    }
}

fn selected_phase() -> Phase {
    match std::env::var("MINISS_T3_ONLY").ok().as_deref() {
        Some("spawn") => Phase::Spawn,
        Some("churn") => Phase::Churn,
        Some("wake") => Phase::Wake,
        Some("udp") => Phase::Udp,
        _ => Phase::All,
    }
}

fn scale_factor() -> usize {
    std::env::var("MINISS_T3_SCALE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

fn wake_pace_us() -> u64 {
    std::env::var("MINISS_T3_WAKE_PACE_US")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(200)
}

fn phase_marker(msg: &str) {
    println!("T3 [phase {msg}]");
}

struct BenchReport {
    spawn_baseline_us: f64,
    sequential_churn_us: f64,
    concurrent_c8_us: f64,
    concurrent_c32_us: f64,
    wake_p50_us: f64,
    wake_p99_us: f64,
    idle_cpu_fraction: f64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let phase = selected_phase();
    let scale = scale_factor();
    let pace_us = wake_pace_us();
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

    if phase == Phase::Udp {
        let iterations = UDP_FLOOR_ITERATIONS * scale;
        let (same_p50, same_p99, same_mean) = udp_pingpong_same_thread(iterations);
        let (cross_p50, cross_p99, cross_mean) = udp_pingpong_cross_thread(iterations);
        println!(
            "T3 quic_conn_lifecycle tokio control (backend={backend}, quinn runtime-tokio) [isolated phase: udp]"
        );
        println!(
            "  PROBE pre-bench idle cpu fraction: {:.4}",
            idle_cpu_fraction_before
        );
        println!(
            "  bare loopback UDP ping-pong same-thread p50/p99/mean: {:.1} / {:.1} / {:.1} µs RTT",
            same_p50, same_p99, same_mean
        );
        println!(
            "  bare loopback UDP ping-pong cross-thread p50/p99/mean: {:.1} / {:.1} / {:.1} µs RTT",
            cross_p50, cross_p99, cross_mean
        );
        println!(
            "  PROBE post-bench idle cpu fraction: {:.4}",
            idle_cpu_probe()
        );
        return;
    }

    let report = run_bench(phase, scale, pace_us).await;

    println!(
        "T3 quic_conn_lifecycle tokio control (backend={backend}, quinn runtime-tokio) [phase: {}]",
        phase.name()
    );
    println!(
        "  PROBE pre-bench idle cpu fraction: {:.4}",
        idle_cpu_fraction_before
    );
    if matches!(phase, Phase::All | Phase::Spawn) {
        println!(
            "  spawn baseline:            {:>9.1} µs/task",
            report.spawn_baseline_us
        );
    }
    if matches!(phase, Phase::All | Phase::Churn) {
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
    }
    if matches!(phase, Phase::All | Phase::Wake) {
        println!(
            "  cross-task wake p50/p99:   {:>9.1} / {:.1} µs",
            report.wake_p50_us, report.wake_p99_us
        );
    }
    println!(
        "  PROBE post-bench idle cpu fraction: {:.4}",
        report.idle_cpu_fraction
    );
    // No pass-line assert: this bench measures, the miniss bench decides.
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

/// PROBE (D22): prove the runtime consumes completions before measuring.
async fn probe_handshake(client: &mut quinn::Endpoint, addr: std::net::SocketAddr) {
    let connection = tokio::time::timeout(
        Duration::from_secs(5),
        client.connect(addr, "localhost").expect("connect"),
    )
    .await
    .expect("probe handshake bounded")
    .expect("probe handshake ok");
    connection.close(0u32.into(), b"probe");
}

async fn run_bench(phase: Phase, scale: usize, wake_pace_us: u64) -> BenchReport {
    let spawns = SPAWN_BASE_TASKS * scale;
    let churn_connections = CHURN_CONNECTIONS * scale;
    let concurrent_connections = CONCURRENT_CONNECTIONS * scale;
    let wake_iterations = WAKE_ITERATIONS * scale;

    let mut report = BenchReport {
        spawn_baseline_us: 0.0,
        sequential_churn_us: 0.0,
        concurrent_c8_us: 0.0,
        concurrent_c32_us: 0.0,
        wake_p50_us: 0.0,
        wake_p99_us: 0.0,
        idle_cpu_fraction: 0.0,
    };

    // Isolated spawn phase: measures the bare executor with no QUIC
    // endpoints in the process at all.
    if phase == Phase::Spawn {
        phase_marker("spawn begin");
        report.spawn_baseline_us = spawn_baseline(spawns).await;
        phase_marker("spawn end");
        report.idle_cpu_fraction = idle_cpu_probe();
        return report;
    }

    // Endpoint bring-up is needed by every phase that touches QUIC.
    {
        let (cert, key) = certs();

        let server = quinn::Endpoint::new(
            Default::default(),
            Some(server_config(&cert, &key)),
            std::net::UdpSocket::bind("127.0.0.1:0").expect("bind server"),
            Arc::new(quinn::TokioRuntime),
        )
        .expect("server endpoint");
        let addr = server.local_addr().expect("server addr");
        let _ = &server; // handle kept alive for the endpoint driver

        // Server accept loop: churn conns are accepted and dropped (implicit
        // close), which is the full server-side lifecycle cost. Mirrors the
        // miniss QuicAcceptor loop, whose next_connection() completes the
        // handshake before handing over the Connection.
        let server_for_acceptor = server.clone();
        tokio::spawn(async move {
            while let Some(incoming) = server_for_acceptor.accept().await {
                let _ = incoming.await;
            }
        });

        let mut client = quinn::Endpoint::new(
            Default::default(),
            None,
            std::net::UdpSocket::bind("127.0.0.1:0").expect("bind client"),
            Arc::new(quinn::TokioRuntime),
        )
        .expect("client endpoint");
        client.set_default_client_config(client_config(&cert));

        // PROBE (D22): completions are consumed.
        probe_handshake(&mut client, addr).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 1. Spawn baseline: spawn + poll + drop of an immediately-completing
        // task, measured from spawn start until every task ran.
        if matches!(phase, Phase::All | Phase::Spawn) {
            if phase != Phase::All {
                phase_marker("spawn begin");
            }
            report.spawn_baseline_us = spawn_baseline(spawns).await;
            if phase != Phase::All {
                phase_marker("spawn end");
            }
        }

        // 2. Sequential handshake churn.
        if matches!(phase, Phase::All | Phase::Churn) {
            if phase != Phase::All {
                phase_marker("seq churn begin");
            }
            let mut sequential_samples = Vec::with_capacity(churn_connections);
            for _ in 0..churn_connections {
                let start = Instant::now();
                let connection = tokio::time::timeout(
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
            report.sequential_churn_us =
                sequential_samples.iter().sum::<f64>() / sequential_samples.len() as f64;
            if phase != Phase::All {
                phase_marker("seq churn end");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // 3. Concurrent churn c8 and c32.
        let concurrent = |client: &quinn::Endpoint, width: usize| {
            let client = client.clone();
            async move {
                let per_task = concurrent_connections / width;
                let start = Instant::now();
                let mut tasks = Vec::with_capacity(width);
                for _ in 0..width {
                    let client = client.clone();
                    tasks.push(async move {
                        for _ in 0..per_task {
                            let connection = tokio::time::timeout(
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
                start.elapsed().as_secs_f64() * 1e6 / concurrent_connections as f64
            }
        };
        if matches!(phase, Phase::All | Phase::Churn) {
            if phase != Phase::All {
                phase_marker("conc churn c8 begin");
            }
            report.concurrent_c8_us = concurrent(&client, 8).await;
            if phase != Phase::All {
                phase_marker("conc churn c8 end");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            if phase != Phase::All {
                phase_marker("conc churn c32 begin");
            }
            report.concurrent_c32_us = concurrent(&client, 32).await;
            if phase != Phase::All {
                phase_marker("conc churn c32 end");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // 4. Cross-task wake: A's traffic wakes B's task. A dedicated endpoint
        // serves the wake phase (the churn acceptor drops every churn
        // connection by design, which would close these two).
        if matches!(phase, Phase::All | Phase::Wake) {
            if phase != Phase::All {
                phase_marker("wake begin");
            }
            let server2 = quinn::Endpoint::new(
                Default::default(),
                Some(server_config(&cert, &key)),
                std::net::UdpSocket::bind("127.0.0.1:0").expect("bind wake server"),
                Arc::new(quinn::TokioRuntime),
            )
            .expect("wake server endpoint");
            let wake_addr = server2.local_addr().expect("wake server addr");
            let read_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let wake_reads = Arc::clone(&read_count);
            // On current_thread tokio::spawn schedules on this same thread —
            // the analogue of miniss's spawn_local relay task.
            tokio::spawn(async move {
                let conn_a = server2
                    .accept()
                    .await
                    .expect("wake conn A incoming")
                    .await
                    .expect("wake conn A");
                let conn_b = server2
                    .accept()
                    .await
                    .expect("wake conn B incoming")
                    .await
                    .expect("wake conn B");
                // Server side: relay one byte from A's uni stream to a
                // server-opened uni stream on B, waking B's driver per byte.
                // The reader paces the writer, so this task stays alive until
                // every byte is consumed and B's stream finishes cleanly.
                let mut incoming = conn_a.accept_uni().await.expect("relay accept");
                let mut out = conn_b.open_uni().await.expect("relay open");
                for _ in 0..wake_iterations {
                    let mut byte = [0u8; 1];
                    incoming.read_exact(&mut byte).await.expect("relay read");
                    out.write_all(&byte).await.expect("relay write");
                }
                let _ = out.finish();
                // Outlive the reader: an early close would reset unread data.
                while wake_reads.load(std::sync::atomic::Ordering::Relaxed) < wake_iterations {
                    tokio::time::sleep(Duration::from_micros(200)).await;
                }
            });

            let conn_a = tokio::time::timeout(
                Duration::from_secs(5),
                client.connect(wake_addr, "localhost").expect("connect"),
            )
            .await
            .expect("wake handshake A bounded")
            .expect("wake handshake A ok");
            let conn_b = tokio::time::timeout(
                Duration::from_secs(5),
                client.connect(wake_addr, "localhost").expect("connect"),
            )
            .await
            .expect("wake handshake B bounded")
            .expect("wake handshake B ok");

            let wake_samples = Arc::new(std::sync::Mutex::new(Vec::with_capacity(wake_iterations)));
            let reader_count = Arc::clone(&read_count);
            let reader_wake = Arc::clone(&wake_samples);
            let reader = async move {
                // The relay opens its server-to-B stream before the first byte;
                // B's driver is woken per relayed byte.
                let mut reply = conn_b.accept_uni().await.expect("client B accept");
                for _ in 0..wake_iterations {
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
                for i in 0..wake_iterations {
                    client_a_stream
                        .write_all(b"x")
                        .await
                        .expect("client A write");
                    // Pace on the reader so the relay cannot complete and close
                    // the connections ahead of B's reads.
                    while read_count.load(std::sync::atomic::Ordering::Relaxed) <= i {
                        tokio::time::sleep(Duration::from_micros(wake_pace_us)).await;
                    }
                }
            })
            .await;
            let mut wake_samples = Arc::try_unwrap(wake_samples)
                .expect("reader completed")
                .into_inner()
                .unwrap();
            wake_samples.sort_unstable();
            report.wake_p50_us = wake_samples[wake_samples.len() / 2] as f64;
            report.wake_p99_us = wake_samples[(wake_samples.len() as f64 * 0.99) as usize] as f64;
            if phase != Phase::All {
                phase_marker("wake end");
            }
        }
    }

    report.idle_cpu_fraction = idle_cpu_probe();
    report
}

/// Spawn baseline: spawn + poll + drop of an immediately-completing task,
/// measured from spawn start until every task ran.
async fn spawn_baseline(spawns: usize) -> f64 {
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let start = Instant::now();
    for _ in 0..spawns {
        let completed = Arc::clone(&completed);
        tokio::spawn(async move {
            completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
    // Await completion: mirror the miniss bench's yield-based wait so both
    // baselines include one completion-poll loop.
    while completed.load(std::sync::atomic::Ordering::Relaxed) < spawns {
        tokio::time::sleep(Duration::from_micros(200)).await;
    }
    start.elapsed().as_secs_f64() * 1e6 / spawns as f64
}

/// Bare loopback UDP ping-pong on one thread with blocking sockets: measures
/// two kernel UDP trips plus four syscalls with no scheduler involvement.
/// This is the floor under the wake chain's kernel share.
fn udp_pingpong_same_thread(iterations: usize) -> (f64, f64, f64) {
    let a = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp floor a");
    let b = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp floor b");
    a.connect(b.local_addr().expect("b addr"))
        .expect("connect a");
    b.connect(a.local_addr().expect("a addr"))
        .expect("connect b");
    let mut buf = [0u8; 1];
    for _ in 0..200.min(iterations) {
        a.send(&buf).expect("floor send");
        b.recv(&mut buf).expect("floor recv");
        b.send(&buf).expect("floor send");
        a.recv(&mut buf).expect("floor recv");
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        a.send(&buf).expect("floor send");
        b.recv(&mut buf).expect("floor recv");
        b.send(&buf).expect("floor send");
        a.recv(&mut buf).expect("floor recv");
        samples.push(start.elapsed().as_micros() as u64);
    }
    percentile3(&mut samples)
}

/// Bare loopback UDP ping-pong across two OS threads: adds the cross-thread
/// scheduler wakeup (what a runtime's waker hop costs at minimum in kernel).
fn udp_pingpong_cross_thread(iterations: usize) -> (f64, f64, f64) {
    let a = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp floor a");
    let b = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp floor b");
    a.connect(b.local_addr().expect("b addr"))
        .expect("connect a");
    b.connect(a.local_addr().expect("a addr"))
        .expect("connect b");
    let handle = std::thread::spawn(move || {
        let mut buf = [0u8; 1];
        for _ in 0..iterations {
            b.recv(&mut buf).expect("floor recv");
            b.send(&buf).expect("floor send");
        }
    });
    let mut buf = [0u8; 1];
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        a.send(&buf).expect("floor send");
        a.recv(&mut buf).expect("floor recv");
        samples.push(start.elapsed().as_micros() as u64);
    }
    handle.join().expect("udp floor thread");
    percentile3(&mut samples)
}

fn percentile3(samples: &mut Vec<u64>) -> (f64, f64, f64) {
    samples.sort_unstable();
    let n = samples.len();
    let p50 = samples[n / 2] as f64;
    let p99 = samples[(n as f64 * 0.99) as usize] as f64;
    let mean = samples.iter().sum::<u64>() as f64 / n as f64;
    (p50, p99, mean)
}
