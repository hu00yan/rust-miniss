//! HTTP echo server for benchmarking the miniss runtime.

use rust_miniss::multicore::MultiCoreRuntime;
use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Buffer;
use rust_miniss::RuntimeMetrics;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

static RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nConnection: keep-alive\r\n\r\nHello from rust-miniss";

async fn handle_client(stream: AsyncTcpStream) {
    let mut buf = Buffer::with_capacity(4096);

    loop {
        match stream.read_rent(buf).await {
            Ok((0, _)) | Err(_) => break,
            Ok((_, returned_buf)) => {
                buf = returned_buf;
                if stream.write_static_all(RESPONSE).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn format_metrics(metrics: &RuntimeMetrics) -> String {
    let mut totals = (
        0usize, // ready_queue_len
        0usize, // max_ready_queue_len
        0u64,   // tasks_spawned
        0u64,   // tasks_completed
        0u64,   // tasks_polled
        0u64,   // io_completions
        0u64,   // messages_processed
        0u64,   // message_batches
        0usize, // max_message_inbox_len
        0u64,   // busy_rounds
        0u64,   // flushes
        0u64,   // flush_total_us
        0u64,   // max_flush_us
        0u64,   // park_flushes
        0u64,   // park_flush_total_us
        0u64,   // max_park_flush_us
        0u64,   // io_completion_batches
        0u64,   // io_completion_total_ns
        0u64,   // max_io_completion_batch_ns
        0usize, // max_io_completion_batch_len
        0u64,   // inline_poll_total_ns
        0u64,   // max_inline_poll_ns
        0u64,   // wake_total_ns
        0u64,   // max_wake_ns
        0u64,   // parks
        0u64,   // park_timeouts
        0u64,   // park_completions
        0u64,   // park_total_us
        0u64,   // max_park_us
        0u64,   // timer_expirations
        0u64,   // inline_completion_hits
        0u64,   // waker_wakeups
    );

    for core in &metrics.cores {
        totals.0 += core.ready_queue_len;
        totals.1 = totals.1.max(core.max_ready_queue_len);
        totals.2 += core.tasks_spawned;
        totals.3 += core.tasks_completed;
        totals.4 += core.tasks_polled;
        totals.5 += core.io_completions;
        totals.6 += core.messages_processed;
        totals.7 += core.message_batches;
        totals.8 = totals.8.max(core.max_message_inbox_len);
        totals.9 += core.busy_rounds;
        totals.10 += core.flushes;
        totals.11 += core.flush_total_us;
        totals.12 = totals.12.max(core.max_flush_us);
        totals.13 += core.park_flushes;
        totals.14 += core.park_flush_total_us;
        totals.15 = totals.15.max(core.max_park_flush_us);
        totals.16 += core.io_completion_batches;
        totals.17 += core.io_completion_total_ns;
        totals.18 = totals.18.max(core.max_io_completion_batch_ns);
        totals.19 = totals.19.max(core.max_io_completion_batch_len);
        totals.20 += core.inline_poll_total_ns;
        totals.21 = totals.21.max(core.max_inline_poll_ns);
        totals.22 += core.wake_total_ns;
        totals.23 = totals.23.max(core.max_wake_ns);
        totals.24 += core.parks;
        totals.25 += core.park_timeouts;
        totals.26 += core.park_completions;
        totals.27 += core.park_total_us;
        totals.28 = totals.28.max(core.max_park_us);
        totals.29 += core.timer_expirations;
        totals.30 += core.inline_completion_hits;
        totals.31 += core.waker_wakeups;
    }

    let mut lines = vec![format!(
        "miniss.metrics cores={} ready={} max_ready={} spawned={} completed={} polled={} completions={} msgs={} msg_batches={} max_inbox={} busy_rounds={} flushes={} flush_total_us={} max_flush_us={} park_flushes={} park_flush_total_us={} max_park_flush_us={} io_completion_batches={} io_completion_total_ns={} max_io_completion_batch_ns={} max_io_completion_batch_len={} inline_poll_total_ns={} max_inline_poll_ns={} wake_total_ns={} max_wake_ns={} parks={} park_timeouts={} park_completions={} park_total_us={} max_park_us={} timer_expirations={} inline_hits={} waker_wakes={}",
        metrics.cores.len(),
        totals.0,
        totals.1,
        totals.2,
        totals.3,
        totals.4,
        totals.5,
        totals.6,
        totals.7,
        totals.8,
        totals.9,
        totals.10,
        totals.11,
        totals.12,
        totals.13,
        totals.14,
        totals.15,
        totals.16,
        totals.17,
        totals.18,
        totals.19,
        totals.20,
        totals.21,
        totals.22,
        totals.23,
        totals.24,
        totals.25,
        totals.26,
        totals.27,
        totals.28,
        totals.29,
        totals.30,
        totals.31,
    )];

    for core in &metrics.cores {
        lines.push(format!(
            "miniss.metrics.core id={} ready={} max_ready={} spawned={} completed={} polled={} completions={} msgs={} msg_batches={} max_inbox={} busy_rounds={} flushes={} flush_total_us={} max_flush_us={} park_flushes={} park_flush_total_us={} max_park_flush_us={} io_completion_batches={} io_completion_total_ns={} max_io_completion_batch_ns={} max_io_completion_batch_len={} inline_poll_total_ns={} max_inline_poll_ns={} wake_total_ns={} max_wake_ns={} parks={} park_timeouts={} park_completions={} park_total_us={} max_park_us={} timer_expirations={} inline_hits={} waker_wakes={}",
            core.core_id,
            core.ready_queue_len,
            core.max_ready_queue_len,
            core.tasks_spawned,
            core.tasks_completed,
            core.tasks_polled,
            core.io_completions,
            core.messages_processed,
            core.message_batches,
            core.max_message_inbox_len,
            core.busy_rounds,
            core.flushes,
            core.flush_total_us,
            core.max_flush_us,
            core.park_flushes,
            core.park_flush_total_us,
            core.max_park_flush_us,
            core.io_completion_batches,
            core.io_completion_total_ns,
            core.max_io_completion_batch_ns,
            core.max_io_completion_batch_len,
            core.inline_poll_total_ns,
            core.max_inline_poll_ns,
            core.wake_total_ns,
            core.max_wake_ns,
            core.parks,
            core.park_timeouts,
            core.park_completions,
            core.park_total_us,
            core.max_park_us,
            core.timer_expirations,
            core.inline_completion_hits,
            core.waker_wakeups,
        ));
    }

    lines.join("\n")
}

fn main() -> io::Result<()> {
    let mut addr = String::from("127.0.0.1:8080");
    let mut cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

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

    let runtime = MultiCoreRuntime::new(Some(cores))
        .map_err(|e| io::Error::other(format!("failed to create runtime: {e}")))?;

    if let Some(interval_ms) = std::env::var("MINISS_METRICS_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|interval_ms| *interval_ms > 0)
    {
        let metrics_runtime = runtime.clone();
        std::thread::Builder::new()
            .name("miniss-metrics".to_string())
            .spawn(move || {
                let interval = Duration::from_millis(interval_ms);
                loop {
                    std::thread::sleep(interval);
                    match metrics_runtime.metrics_snapshot(Duration::from_secs(1)) {
                        Ok(metrics) => {
                            eprintln!("{}", format_metrics(&metrics));
                        }
                        Err(err) => {
                            eprintln!("miniss.metrics error={err}");
                        }
                    }
                }
            })
            .map_err(|e| io::Error::other(format!("failed to spawn metrics reporter: {e}")))?;
    }

    for core_id in 0..cores {
        let worker_runtime = runtime.clone();
        runtime
            .spawn_on(core_id, async move {
                let mut listener = match AsyncTcpListener::bind_reuse_port(addr) {
                    Ok(listener) => listener,
                    Err(e) => {
                        eprintln!("bind error on core {core_id}: {e}");
                        return;
                    }
                };

                println!("rust-miniss http_echo listening on {addr} with {cores} worker cores");
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            if let Err(e) = worker_runtime.spawn(handle_client(stream)) {
                                eprintln!("spawn error on core {core_id}: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("accept error on core {core_id}: {e}");
                            break;
                        }
                    }
                }
            })
            .map_err(|e| io::Error::other(format!("failed to spawn accept loop: {e}")))?;
    }

    runtime.block_on(futures::future::pending::<()>());
    let _ = runtime.shutdown();
    Ok(())
}
