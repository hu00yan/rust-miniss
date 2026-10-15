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
| close 8 | 127,143 | (data) | (data) | 161 us | (data) |
| close 32 | 326,474 | (data) | (data) | 4.26 ms | (data) |
| close 800 | 730,778 | (data) | (data) | 9.20 ms | (data) |

### Key observations from the baseline:

- **keep-alive 800**: miniss achieves peak RPS (816,255) while monoio trails at 816,039
- **keep-alive 200**: miniss leads at 760,140 vs monoio's 734,245, with tokio having best p99 (0.87 ms)
- **keep-alive 32**: monoio achieves peak RPS (493,650) vs miniss's 399,792, with compio having best p99 (237 us)
- **keep-alive 8**: glommio leads RPS (237,549) vs miniss/monoio (152K/150K), with best p99 at 133 us
- **close scenarios**: miniss data available; other runtimes not in single-round matrix

The verdict from the comparison was "failed for stability, not correctness":
miniss had a 1.56 ms p99 in one round while its other two rounds were 155 us and 191 us.
These samples are useful for diagnosing tail instability, but are not sufficient
to claim runtime parity.

Raw artifacts are retained under `.bench-http-20260714*` directories:
`summary.csv`, `metadata.json`, wrk output, server logs, and verdict JSON/CSV.
See `docs/review/performance-baseline-2026-07-14.md` and
`docs/review/performance-baseline-2026-07-11.md` for full details.

## Metrics

Every `MultiCoreRuntime` owns one control-plane aggregator thread, started in
`MultiCoreRuntime::new` and joined when the runtime drops. There is no
process-global registry — see `src/metrics.rs` for why a global is both
forbidden by `AGENTS.md` MUST NOT #4 and wrong for a thread-per-core runtime.

```rust
// Serve this from any /metrics handler you own; the runtime only owns numbers.
let body = runtime.metrics_view()?.render_prometheus();

// Or force a fresh, bounded aggregation pass instead of reading the last one.
let body = runtime.metrics_refresh(Duration::from_secs(1))?.render_prometheus();
```

Each exported metric is emitted as `# HELP` / `# TYPE`, one
`name{shard="N"} value` line per shard, and one unlabelled `name value` line
holding the runtime-wide merge (sum for counters and gauges, max for
high-water marks). The interval defaults to 1s and is set at startup with
`MINISS_METRICS_INTERVAL_MS`.

See `examples/metrics_export.rs` for a runnable scrape endpoint.

Performance is tracked as reproducible Linux snapshots, not as a permanent
throughput claim. See [the current baseline](docs/review/performance-baseline-2026-07-15.md).
