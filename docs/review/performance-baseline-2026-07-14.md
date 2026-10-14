# HTTP baseline artifact — 2026-07-14

This is a reproducible miniss-only smoke baseline, not a runtime ranking.

Environment and workload:

- Orb Linux `7.0.11-orbstack-00360-gc9bc4d96ac70`, 8 vCPUs.
- Server CPUs `0-3`, client CPUs `4-7`, `taskset` placement.
- `HTTP_CORES=4`, 4 wrk threads, 32 keep-alive connections, 5 seconds.
- Three sequential rounds; no socket errors, HTTP errors, or process leaks.

| Round | Requests/s | p50 | p99 |
|---|---:|---:|---:|
| 1 | 400,459.17 | 64 us | 206 us |
| 2 | 402,094.08 | 64 us | 189 us |
| 3 | 340,872.10 | 73 us | 1.12 ms |

`bench_verdict.pl` passed with 3 rounds: median 400,459.17 RPS, RPS CV
7.47%, median p99 206 us, and p99 MAD ratio below the configured limit.

The matching `Connection: close`, 32-connection matrix also passed its
three-round verdict: median 384,214.06 RPS, RPS CV 1.44%, median p99 230 us,
and no errors or leaks.

The first three-round same-workload comparison with monoio, compio, and tokio
also completed without errors or leaks:

| Runtime | Median RPS | RPS CV | Median p99 | Verdict |
|---|---:|---:|---:|---|
| miniss | 378,424 | 5.52% | 401 us | rejected: p99 MAD |
| monoio | 457,205 | 4.21% | 584 us | pass |
| compio | 446,473 | 1.20% | 543 us | pass |
| tokio | 402,909 | 1.07% | 191 us | pass |
| glommio | 296,271 | 3.55% | 1.78 ms | pass |

The comparison verdict is therefore failed for stability, not correctness:
miniss had a 1.56 ms p99 in one round while its other two rounds were 155 us
and 191 us. These samples are useful for diagnosing tail instability, but are
not sufficient to claim runtime parity.

A light workload-shape matrix also completed (one round per scenario):

| Scenario | miniss RPS | p99 |
|---|---:|---:|
| keep-alive 8 | 125,031 | 230 us |
| keep-alive 32 | 347,813 | 379 us |
| keep-alive 200 | 646,716 | 3.83 ms |
| keep-alive 800 | 705,089 | 10.64 ms |
| close 8 | 127,143 | 161 us |
| close 32 | 326,474 | 4.26 ms |
| close 800 | 730,778 | 9.20 ms |

Only the `ka200` scenario in this matrix ran the four-runtime comparison;
the other scenarios were miniss-only. These are shape-coverage samples, not
stability verdicts.

Raw artifacts are retained under `.scratch/bench-http-20260714-miniss-ka32*` and
`.scratch/bench-http-20260714-miniss-close32-*`: `summary.csv`, `metadata.json`,
wrk output, server logs, and verdict JSON/CSV. Runtime comparison artifacts are
under `.scratch/bench-http-20260714-runtime-ka32*` and
`.scratch/bench-http-20260714-glommio-ka32*`.
The VM reported no CPU model and produced no usable perf-event log, so the
CPU-counter claim remains open; the five-round server/client runtime sample is
recorded below.

## Full runtime shape matrix — 2026-07-15

Orb Linux repeated the complete shape matrix once with the same placement:
server CPUs `0-3`, client CPUs `4-7`, `HTTP_CORES=4`, eight wrk threads, and
the runtime set miniss/monoio/compio/tokio/glommio. Keep-alive used 8/32/200/800
connections; close used 8/32/800 connections. The 5-second cases ran for 5
seconds and the 800-connection cases for 10 seconds. All samples reported zero
socket errors, HTTP errors, and exit leaks.

| Scenario | miniss RPS | monoio RPS | Best runtime RPS | miniss p99 | Best p99 |
|---|---:|---:|---:|---:|---:|
| keep-alive 8 | 152,040 | 150,217 | 237,549 (glommio) | 121 us | 133 us |
| keep-alive 32 | 399,792 | 493,650 | 493,650 (monoio) | 273 us | 237 us (compio) |
| keep-alive 200 | 760,140 | 734,245 | 760,140 (miniss) | 3.69 ms | 0.87 ms (tokio) |
| keep-alive 800 | 816,255 | 816,039 | 816,255 (miniss) | 8.56 ms | 4.55 ms (monoio) |
| close 8 | 134,749 | 151,675 | 275,758 (glommio) | 135 us | 130 us (monoio) |
| close 32 | 403,802 | 480,935 | 480,935 (monoio) | 216 us | 182 us (tokio) |
| close 800 | 848,422 | 831,552 | 848,422 (miniss) | 9.41 ms | 4.32 ms (tokio) |

This is shape evidence, not a stable ranking: it has one round per runtime and
does not include task-clock/perf counters. It supports the narrower statement
that miniss is competitive at 200–800 connections in this environment, while
8–32 connections and tail latency still vary materially by runtime. Raw
artifacts are under `.scratch/bench-http-matrix-20260715-full/`; the machine
readable inputs are `matrix.csv` and `ratios.csv`.

## Five-round runtime stability sample — 2026-07-15

The `BENCH_MATRIX_ROUNDS=5` driver ran `ka32` and `close32` on Orb Linux with
server CPUs `0-3`, client CPUs `4-7`, four miniss workers, eight wrk threads,
and all five runtimes. Every one of the 50 samples had zero socket errors,
HTTP errors, and process leaks.

| Scenario | Runtime | Median RPS | RPS CV | Median p99 | Verdict |
|---|---|---:|---:|---:|---|
| ka32 | miniss | 407,550 | 1.22% | 232 us | pass |
| ka32 | monoio | 489,076 | 1.44% | 487 us | pass |
| ka32 | compio | 483,846 | 2.78% | 497 us | pass |
| ka32 | tokio | 357,526 | 1.11% | 189 us | pass |
| ka32 | glommio | 399,798 | 7.18% | 1.83 ms | pass |
| close32 | miniss | 402,273 | 1.40% | 230 us | pass |
| close32 | monoio | 481,799 | 3.74% | 464 us | pass |
| close32 | compio | 475,495 | 3.20% | 498 us | pass |
| close32 | tokio | 354,612 | 0.37% | 187 us | pass |
| close32 | glommio | 381,049 | 10.92% | 2.02 ms | rejected: CV |

The close32 aggregate verdict is rejected solely because glommio exceeded the
10% CV threshold; the sample was retained. These results do not establish
runtime parity: miniss is approximately 16–17% below monoio/compio throughput
in these 32-connection samples. Raw artifacts and verdict JSON/CSV are under
`.scratch/bench-http-matrix-20260715-5round/`.

## Topology smoke coverage — 2026-07-15

The topology driver also covered worker `1/2/4` × client threads `1/2/4` for
ka8 and close8 with server CPUs `0-3` and client CPUs `4-7`. This produced 90
runtime samples, all with zero socket errors, HTTP errors, and process leaks.
They are retained under `.scratch/bench-http-topology-1-2-4/`; they are
coverage samples rather than multi-round ranking evidence. Worker=8 is not
claimed because this 8-vCPU host cannot isolate eight server CPUs from a local
client.

The same host also ran worker `1/2/4` with one client thread for ka200 and
close32, producing 30 additional high-load runtime samples with zero errors or
leaks under `.scratch/bench-http-topology-high-1-2-4/`. Relative runtime order
changes across worker counts, so these samples remain separate from the
five-round baseline rather than being averaged into it.

## Metrics collection smoke — 2026-07-15

An isolated miniss `ka32` run with `HTTP_COLLECT_METRICS=1` collected a maximum
RSS of 509,916 KiB in
`.scratch/bench-http-metrics-smoke/miniss.rss.txt`. The raw perf file is also
retained, but Orb denied the syscall tracepoints under `/sys/kernel/tracing`,
so it contains the explicit `perf` permission error and no task-clock/syscall
values. This is diagnostic evidence, not a completed perf-counter baseline;
privileged or differently configured Linux is still required.
