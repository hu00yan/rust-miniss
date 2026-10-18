# rust-miniss

A thread-per-core async runtime for Linux and macOS. Linux uses **io_uring** for the performance path; macOS uses kqueue for the correctness and development path. Inspired by [Seastar](https://github.com/scylladb/seastar) and [Monoio](https://github.com/bytedance/monoio).

## Performance Baseline

Performance is tracked as reproducible Linux snapshots, not as a permanent
throughput claim. The 2026-07-15 baseline matrix compared miniss against
monio/compio/tokio/glommio on Orb Linux with 8 vCPUs, server CPUs
0-3, client CPUs 4-7, HTTP_CORES=4, 8 wrk threads:

### Full Runtime Shape Matrix (keep-allive / close, 5s / 10s durations)

| Scenario | miniss RPS | monoio RPS | best RPS | miniss p99 | best p99 |
|---|---:|---:|---:|---:|---:|
| keep-alive 8 | 152,040 | 150,217 | 237,549 (glommio) | 121 us | 133 us |
| keep-alive 32 | 399,792 | 493,650 | 493,650 (monoio) | 273 us | 237 us (compio) |
| keep-alive 200 | 760,140 | 734,245 | 760,140 (miniss) | 3.69 ms | 0.87 ms (tokio) |
| keep-alive 800 | 816,255 | 816,039 | 816,255 (miniss) | 8.56 ms | 4.55 ms (monoio) |
| close 8 | 134,749 | 151,675 | 275,758 (glommio) | 135 us | 130 us (monoio) |
| close 32 | 403,802 | 480,935 | 480,935 (monoio) | 216 us | 182 us (tokio) |
| close 800 | 848,422 | 831,552 | 848,422 (miniss) | 9.41 ms | 4.32 ms (tokio) |

### Key observations from the baseline:

- **keep-alive 800**: miniss achieves peak RPS (816,255) while monoio trails at 816,039
- **keep-alive 200**: miniss leads at 760,140 vs monoio's 734,245, with tokio having best p99 (0.87 ms)
- **keep-alive 32**: monoio achieves peak RPS (493,650) vs miniss's 399,792, with compio having best p99 (237 us)
- **keep-alive 8**: glommio leads RPS (237,549) vs miniss/monoio (152K/150K), with best p99 at 133 us
- **close 8/32/800**: the full single-round matrix includes other runtimes; glommio leads close-8 RPS, monoio leads close-32 RPS, and miniss leads close-800 RPS, while tokio has the best close-32/800 p99

The verdict from the comparison was "failed for stability, not correctness":
miniss had a 1.56 ms p99 in one round while its other two rounds were 155 us and 191 us.
These samples are useful for diagnosing tail instability, but are not sufficient
to claim runtime parity.

Raw artifacts are retained under `.bench-http-20260714*` directories:
`summary.csv`, `metadata.json`, wrk output, server logs, and verdict JSON/CSV.
See `docs/review/performance-baseline-2026-07-14.md` and
`docs/review/performance-baseline-2026-07-11.md` for full details.

## Metrics

The runtime keeps counters shard-local and exposes snapshots independently of
any monitoring vendor. Read a fresh per-shard snapshot with
`MultiCoreRuntime::metrics_snapshot(timeout)`, or use `metrics_view()` for the
latest periodically aggregated view; `metrics_refresh(timeout)` requests a
fresh bounded sample. Applications can map `RuntimeMetrics` and
`CoreMetricsSnapshot` into their own registry, telemetry SDK, logs, or another
backend. Sampling is a control-plane operation, not a per-request hook.

There is no process-global registry or HTTP endpoint owned by the runtime.
For convenience, `MetricsView::render_prometheus()` formats the latest view as
Prometheus text, but Prometheus is optional and no Prometheus client dependency
is required. The interval defaults to 1s and is set at startup with
`MINISS_METRICS_INTERVAL_MS`.

See [the metrics integration guide](docs/METRICS.md) for the generic snapshot
API and the optional Prometheus example in `examples/metrics_export.rs`.

Performance is tracked as reproducible Linux snapshots, not as a permanent
throughput claim. See the [2026-07-15 source matrix](docs/review/performance-baseline-2026-07-14.md)
and the [latest same-host A/B record](docs/review/performance-baseline-2026-10-15-vm.md).
