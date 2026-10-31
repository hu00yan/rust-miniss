//! Export runtime metrics in Prometheus text exposition format.
//!
//! The metrics control plane is owned by the [`MultiCoreRuntime`]: workers
//! push their shard-local counters into single-writer slots at the event-loop
//! boundary, and a scrape reads those slots directly on the calling thread —
//! no sampler thread exists and no process-global registry is involved.  This
//! example picks the documented API route — the embedder owns the HTTP
//! endpoint, the runtime only owns the numbers.
//!
//! Run with:
//!
//! ```text
//! cargo run --example metrics_export -- 127.0.0.1:9090
//! curl -s http://127.0.0.1:9090/metrics
//! ```
//!
//! Wiring a real exporter is one line: serve `runtime.metrics_view()` rendered
//! as text with `Content-Type: text/plain; version=0.0.4`.

use rust_miniss::MultiCoreRuntime;
use std::io::{Read, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// How long the example keeps its endpoint up.
const SERVE_WINDOW: Duration = Duration::from_secs(2);

/// Render one scrape response for `runtime`.
///
/// This never wakes a worker: the view is assembled inline from the per-shard
/// publication slots, so a fast scrape loop costs the data plane nothing.
fn scrape_response(runtime: &MultiCoreRuntime) -> Vec<u8> {
    let body = match runtime.metrics_view() {
        Ok(view) => view.render_prometheus(),
        Err(err) => format!("# miniss metrics unavailable: {err}\n"),
    };
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Answer one already-accepted scrape connection.
fn answer_scrape(mut stream: TcpStream, runtime: &MultiCoreRuntime) {
    let mut request = [0u8; 1024];
    // Bounded: only the request line matters, then answer and close.
    let _ = stream.read(&mut request);
    let _ = stream.write_all(&scrape_response(runtime));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9090".into());

    // One runtime; workers publish to their slots at the loop boundary and
    // this process holds no control-plane thread beyond them.
    let runtime = MultiCoreRuntime::new(Some(2))?;
    println!(
        "workers publish per loop turn; embedder scrape cadence reference {:?}",
        runtime.metrics_interval()
    );

    // The embedder owns the endpoint. Non-blocking so this thread can also
    // generate traffic and close the listener without a helper thread.
    let listener = TcpListener::bind(&bind)?;
    listener.set_nonblocking(true)?;
    println!("serving http://{bind}/metrics");

    // Drive traffic first, so the first scrape has something to report.
    let traffic = runtime.cpu_count();
    let (tx, rx) = std::sync::mpsc::channel();
    for core_id in 0..traffic {
        let tx = tx.clone();
        runtime.spawn_on(core_id, async move {
            tx.send(()).ok();
        })?;
    }
    drop(tx);
    for _ in 0..traffic {
        rx.recv_timeout(Duration::from_secs(5))?;
    }

    // Serve scrapes for a bounded window, then shut the endpoint down cleanly.
    let deadline = Instant::now() + SERVE_WINDOW;
    let mut served = 0usize;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                answer_scrape(stream, &runtime);
                served += 1;
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err.into()),
        }
    }
    drop(listener);
    println!("answered {served} scrape(s) over {traffic} shards");

    // The same body the endpoint returns, forced fresh for the console.
    print!(
        "{}",
        runtime
            .metrics_refresh(Duration::from_secs(2))?
            .render_prometheus()
    );

    runtime.shutdown_with_timeout(Duration::from_secs(5))?;
    runtime.join()?;
    // Dropping the runtime joins its workers and releases the slots.
    drop(runtime);
    Ok(())
}
