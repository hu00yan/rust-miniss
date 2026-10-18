# Runtime metrics and integrations

`MultiCoreRuntime` owns shard-local counters and aggregates them on a control-plane
thread. The runtime does not own a process-global registry or a metrics HTTP
endpoint. Applications choose how to consume or export the snapshots.

## Provider-neutral snapshot API

Use `metrics_snapshot` when the application wants a fresh bounded collection of
all shard values:

```rust,no_run
use rust_miniss::MultiCoreRuntime;
use std::time::Duration;

let runtime = MultiCoreRuntime::new(Some(2)).unwrap();
let snapshot = runtime.metrics_snapshot(Duration::from_secs(1)).unwrap();
for core in &snapshot.cores {
    println!("core={} spawned={} completed={}", core.core_id, core.tasks_spawned, core.tasks_completed);
}
```

`RuntimeMetrics` contains the per-shard `CoreMetricsSnapshot` values. These
public data structures are the general integration surface: an application can
map them to its chosen metrics SDK, telemetry pipeline, diagnostics endpoint, or
logs without requiring the runtime to know that backend. Sampling is bounded
control-plane work; do not call it in a per-request hot path.

For periodic collection, `metrics_view()` returns the latest successful view
from the runtime-owned sampler, including its sample time, duration, and
consecutive failure count. `metrics_refresh(timeout)` asks that sampler for a
fresh bounded pass. The default periodic interval is 1 second and can be
configured at startup with `MINISS_METRICS_INTERVAL_MS`.

## Optional Prometheus text exposition

`MetricsView::render_prometheus()` is a dependency-free convenience formatter,
not a requirement of the snapshot API. The runtime does not register itself in
a process-global Prometheus registry and does not start an HTTP server. The
application owns the endpoint, scrape policy, access control, and lifecycle.

For a runnable minimal endpoint and the expected content type, see
[`examples/metrics_export.rs`](../examples/metrics_export.rs). A Prometheus
server can scrape that application-owned endpoint, for example:

```yaml
scrape_configs:
  - job_name: miniss
    metrics_path: /metrics
    static_configs:
      - targets: ["127.0.0.1:9090"]
```

The text formatter emits one sample per shard with a `shard` label plus an
unlabelled runtime-wide value. Counters merge by sum. Gauges use the merge rule
of each descriptor: additive gauges sum, while high-water marks use max. Users
who need a different naming, label, registry, or exposition format can consume
the generic snapshot values and implement that mapping in their application.
