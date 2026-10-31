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

const SPAWN_BASE_TASKS: usize = 20_000;
const CHURN_CONNECTIONS: usize = 100;
const CONCURRENT_CONNECTIONS: usize = 200;
const WAKE_ITERATIONS: usize = 500;
const PASS_LINE_MICROS: f64 = 100.0;
const UDP_FLOOR_ITERATIONS: usize = 2_000;

// ---------------------------------------------------------------------------
// Diagnostic knobs. Every knob defaults to the unmodified T3 behavior; the
// full bench output is byte-identical when none is set. They exist so a
// profiler (macOS `sample`) can be attached to ONE phase at a time and to a
// phase long enough to outlast the sampling window:
//
//   MINISS_T3_ONLY=spawn|churn|wake|udp
//       Run only that phase and skip the pass-line assert (the PROBE idle
//       checks still run and are printed). `udp` is a bare loopback
//       ping-pong with no runtime in the loop: the kernel floor under the
//       cross-task wake chain.
//   MINISS_T3_SCALE=<n>
//       Multiply every iteration count by n (default 1). Used to make an
//       isolated phase outlast a `sample` window.
//   MINISS_T3_WAKE_SCALE=<n>
//       Wake-phase-only iteration multiplier (default 1): the spawn phase
//       is capped by the 65,536-slot local admission queue so SCALE <= 3,
//       but profiling the full-context wake phase needs a longer phase.
//   MINISS_T3_WAKE_PACE_US=<µs>
//       Writer pacing interval for the wake phase (default 200). Lower it to
//       shrink the pacing share of the measured wake latency when the raw
//       relay chain is the quantity of interest. NOTE: miniss's timer wheel
//       has 1 ms resolution, so any pace below ~1 ms still quantizes to the
//       next 1 ms tick; the knob exists to measure that quantization, not to
//       escape it.
// ---------------------------------------------------------------------------

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

/// Wake-phase-only multiplier (default 1). The spawn phase is capped by the
/// 65,536-slot local admission queue, so MINISS_T3_SCALE cannot exceed 3 in
/// the full bench; profiling the full-context wake phase needs a longer
/// phase than that, hence this separate knob.
fn wake_scale_factor() -> usize {
    std::env::var("MINISS_T3_WAKE_SCALE")
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

/// Marker for profiling drivers: emitted only when a phase is isolated
/// (MINISS_T3_ONLY set), so the default run's stdout stays unchanged.
/// Phase markers go to stderr (unbuffered, so a profiling driver can attach
/// a sampler while the phase is still running) and ONLY when a diagnostic
/// knob is active: with no env var set, stdout AND stderr are byte-identical
/// to the pre-tooling bench.
fn phase_marker(markers: bool, msg: &str) {
    if markers {
        eprintln!("T3 [phase {msg}]");
    }
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

fn main() {
    let phase = selected_phase();
    let scale = scale_factor();
    let wake_scale = wake_scale_factor();
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
        // Bare loopback floor with no runtime involved: same-thread blocking
        // ping-pong (syscall + kernel path only) and cross-thread ping-pong
        // (adds the scheduler wakeup). These bound the kernel share of the
        // cross-task wake chain, which makes two UDP loopback trips per byte.
        let iterations = UDP_FLOOR_ITERATIONS * scale;
        let (same_p50, same_p99, same_mean) = udp_pingpong_same_thread(iterations);
        let (cross_p50, cross_p99, cross_mean) = udp_pingpong_cross_thread(iterations);
        println!("T3 quic_conn_lifecycle (backend={backend}) [isolated phase: udp]");
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

    let runtime = MultiCoreRuntime::new(Some(1)).expect("one-shard runtime");
    let (tx, rx) = std::sync::mpsc::channel();
    let runtime_for_bench = Arc::clone(&runtime);
    runtime
        .spawn_on(0, {
            let tx = tx.clone();
            async move {
                let report = run_bench(runtime_for_bench, phase, scale, wake_scale, pace_us).await;
                let _ = tx.send(report);
            }
        })
        .expect("admit bench task");

    let report = rx
        .recv_timeout(Duration::from_secs(120))
        .expect("bench completed");

    if phase == Phase::All {
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
    } else {
        // Isolated-phase output: probes printed, pass-line assert skipped
        // (a profiler attaches to one phase only; the go/no-go decision is
        // the full bench's job).
        println!(
            "T3 quic_conn_lifecycle (backend={backend}) [isolated phase: {}]",
            phase.name()
        );
        println!(
            "  PROBE pre-bench idle cpu fraction: {:.4}",
            idle_cpu_fraction_before
        );
        if phase == Phase::Spawn {
            println!(
                "  spawn baseline:            {:>9.1} µs/task",
                report.spawn_baseline_us
            );
        }
        if phase == Phase::Churn {
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
        if phase == Phase::Wake {
            println!(
                "  cross-task wake p50/p99:   {:>9.1} / {:.1} µs",
                report.wake_p50_us, report.wake_p99_us
            );
        }
        println!(
            "  PROBE post-bench idle cpu fraction: {:.4}",
            report.idle_cpu_fraction
        );
    }
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

async fn run_bench(
    runtime: Arc<MultiCoreRuntime>,
    phase: Phase,
    scale: usize,
    wake_scale: usize,
    wake_pace_us: u64,
) -> BenchReport {
    // Markers are diagnostics: emit them iff any knob deviates from the
    // default so the plain run's output streams stay byte-identical.
    let markers = phase != Phase::All || scale != 1 || wake_scale != 1 || wake_pace_us != 200;
    let spawns = SPAWN_BASE_TASKS * scale;
    let churn_connections = CHURN_CONNECTIONS * scale;
    let concurrent_connections = CONCURRENT_CONNECTIONS * scale;
    let wake_iterations = WAKE_ITERATIONS * scale * wake_scale;

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
        phase_marker(markers, "spawn begin");
        report.spawn_baseline_us = spawn_baseline(&runtime, spawns).await;
        phase_marker(markers, "spawn end");
        report.idle_cpu_fraction = idle_cpu_probe();
        return report;
    }

    // Endpoint bring-up is needed by every phase that touches QUIC; the
    // isolated spawn phase measures the bare executor and skips it.
    {
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
        if matches!(phase, Phase::All | Phase::Spawn) {
            phase_marker(markers, "spawn begin");
            report.spawn_baseline_us = spawn_baseline(&runtime, spawns).await;
            phase_marker(markers, "spawn end");
        }

        // 2. Sequential handshake churn.
        if matches!(phase, Phase::All | Phase::Churn) {
            phase_marker(markers, "seq churn begin");
            let mut sequential_samples = Vec::with_capacity(churn_connections);
            for _ in 0..churn_connections {
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
            report.sequential_churn_us =
                sequential_samples.iter().sum::<f64>() / sequential_samples.len() as f64;
            phase_marker(markers, "seq churn end");
            rust_miniss::timer::sleep(Duration::from_millis(100)).await;
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
                start.elapsed().as_secs_f64() * 1e6 / concurrent_connections as f64
            }
        };
        if matches!(phase, Phase::All | Phase::Churn) {
            phase_marker(markers, "conc churn c8 begin");
            report.concurrent_c8_us = concurrent(&client, 8).await;
            phase_marker(markers, "conc churn c8 end");
            rust_miniss::timer::sleep(Duration::from_millis(100)).await;
            phase_marker(markers, "conc churn c32 begin");
            report.concurrent_c32_us = concurrent(&client, 32).await;
            phase_marker(markers, "conc churn c32 end");
            rust_miniss::timer::sleep(Duration::from_millis(100)).await;
        }

        // 4. Cross-task wake: A's traffic wakes B's task. A dedicated endpoint
        // serves the wake phase (the churn acceptor drops every churn
        // connection by design, which would close these two).
        if matches!(phase, Phase::All | Phase::Wake) {
            phase_marker(markers, "wake begin");
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
                    for _ in 0..wake_iterations {
                        let mut byte = [0u8; 1];
                        incoming.read_exact(&mut byte).await.expect("relay read");
                        out.write_all(&byte).await.expect("relay write");
                    }
                    let _ = out.finish();
                    // Outlive the reader: an early close would reset unread data.
                    while wake_reads.load(std::sync::atomic::Ordering::Relaxed) < wake_iterations {
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
                        rust_miniss::timer::sleep(Duration::from_micros(wake_pace_us)).await;
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
            phase_marker(markers, "wake end");
        }
    }

    report.idle_cpu_fraction = idle_cpu_probe();
    report
}

/// Spawn baseline: spawn + poll + drop of an immediately-completing task,
/// measured from spawn start until every task ran.
async fn spawn_baseline(runtime: &Arc<MultiCoreRuntime>, spawns: usize) -> f64 {
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let start = Instant::now();
    for _ in 0..spawns {
        let completed = Arc::clone(&completed);
        runtime
            .spawn_local(async move {
                completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
            .expect("spawn baseline task");
    }
    // Await completion: the spawned tasks run on this same shard, so the
    // wait must yield to the executor rather than spin the thread.
    while completed.load(std::sync::atomic::Ordering::Relaxed) < spawns {
        rust_miniss::timer::sleep(Duration::from_micros(200)).await;
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
