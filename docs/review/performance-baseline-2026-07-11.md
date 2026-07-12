# Linux Runtime Performance Snapshot — 2026-07-11/12

Environment: Orb Ubuntu, Linux 7.0.11-orbstack, aarch64, 8 vCPU. HTTP results
use 800 connections, 8 wrk threads, 10 seconds, and include throughput plus
p99. This is a snapshot, not a production-safety claim.

## Superseded exploratory comparison

The first three-run medians were 173,967 RPS for miniss, 269,878 for Monoio,
and 192,660 for Compio. One run reported 509 read errors. Those results used
the deleted independent onecore example and an incorrect one-read/one-request
echo loop. They remain historical diagnostics, not a current baseline.

Raw data: `.scratch/http-current-vs-mature-20260711{,-r2,-r3}/`.

## Unified-server comparison after multicore scheduler repair

All miniss rows use the same `http_echo` binary; `HTTP_CORES` is the only
single/multicore switch. Monoio and Compio receive the same worker count. All
servers share the same bounded HTTP-header framer and 1024 listen backlog.
There were no wrk socket errors in these nine runs.

Three-run medians:

| Server workers | Runtime | RPS | p99 | miniss throughput gap |
|---:|---|---:|---:|---:|
| 1 | miniss | 204,252 | 8.62 ms | — |
| 1 | monoio | 352,891 | 38.49 ms | -42.1% |
| 1 | compio | 250,261 | 91.12 ms | -18.4% |
| 4 | miniss | 428,153 | 6.70 ms | — |
| 4 | monoio | 656,821 | 7.50 ms | -34.8% |
| 4 | compio | 633,702 | 7.02 ms | -32.4% |
| 8 | miniss | 366,646 | 19.82 ms | — |
| 8 | monoio | 662,529 | 11.01 ms | -44.7% |
| 8 | compio | 762,589 | 11.07 ms | -51.9% |

Raw data: `.scratch/http-unified-{1w,4w,8w}-r{1,2,3}/`.

The 8-worker case oversubscribes this 8-vCPU VM once the eight wrk client
threads are included, so it measures whole-VM contention as well as server
scaling. The 4-worker result is the primary multicore comparison on this host.

Before repair, the 4-worker miniss run completed only 97 requests (9.6 RPS).
`spawn_local` kept local futures in a vector, restarted polling at index zero,
and used a no-op waker, permanently starving tasks above the 64-poll budget.
Local tasks now enter the same generational TaskArena and ready queue as other
shard-owned tasks. The identical 4-worker/800-connection smoke then reached
417,179 RPS with 7.63 ms p99. A 256-local-task regression test mechanically
guards the former starvation boundary.

## Descriptor-lease hotspot

Low-overhead syscall tracing found approximately one `dup` plus one `close`
per HTTP request. A three-second traced comparison showed:

| Build | RPS under strace | dup calls | close calls |
|---|---:|---:|---:|
| 6922a07 baseline | 46,737 | 140,676 | 140,781 |
| connection registration plus operation Arc lease | 118,921 | 909 | 1,014 |

The traced RPS is not a normal throughput measurement; it establishes syscall
attribution. The remaining calls are primarily connection-lifecycle work.

An alternating three-pair A/B against commit 6922a07 improved median RPS from
219,055 to 226,156 (+3.2%) and p99 from 6.32 to 5.92 ms (-6.3%). Raw data:
`.scratch/http-lease-ab-{base,opt}-{1,2,3}/`. This old-binary A/B remains valid
only for narrow descriptor-syscall attribution; its absolute throughput must
not be compared with the corrected unified-server results.

## Next candidates

1. Make diagnostics report perf collection failure instead of writing zeros.
2. Profile the corrected 4-worker main path with runtime metrics.
3. Migrate other TCP operations to the registered descriptor path, one at a
   time with A/B evidence.
4. Only then evaluate io_uring registered/fixed files with explicit capability
   probing and generation-safe slot reuse.

## io_uring task-run flag experiments

Four-worker, 800-connection, three-run medians against the corrected default
baseline of 428,153 RPS / 6.70 ms p99:

| Ring flags | RPS | p99 | RPS change | Decision |
|---|---:|---:|---:|---|
| `SINGLE_ISSUER` | 463,885 | 6.00 ms | +8.3% | reject as inferior |
| `COOP_TASKRUN` | 479,504 | 5.24 ms | +12.0% | enable with fallback |
| `SINGLE_ISSUER+COOP_TASKRUN` | 469,167 | 6.56 ms | +9.6% | reject as inferior |
| `SINGLE_ISSUER+DEFER_TASKRUN` | 26–28k | 12–13 ms | about -94% | reject; timeouts observed |

Raw data: `.scratch/http-uring-{single,coop,single-coop,defer}-r*`.
`DEFER_TASKRUN` is not compatible with the current GETEVENTS/park protocol and
must not be enabled merely because the kernel supports the setup flag.

An alternating production-default A/B gave paired throughput changes of
+8.35%, -0.46%, and +3.34% (paired median +3.34%). Across the three runs,
COOP on/off medians were 497,011 and 466,650 RPS. Raw data:
`.scratch/http-coop-ab-{on,off}-r{1,2,3}/`.

Privileged perf tracepoint measurements with a raised 65,536 fd limit showed
that kernel-entry frequency is not the remaining gap:

| Runtime | Requests | `io_uring_enter` | requests/enter |
|---|---:|---:|---:|
| miniss COOP | 5,450,303 | 128,457 | 42.4 |
| miniss plain | 5,407,957 | 120,237 | 45.0 |
| Monoio | 7,957,365 | 189,269 | 42.0 |
| Compio | 7,677,354 | 240,649 | 31.9 |

COOP improves task-work timing, not syscall count. Further submission batching
is therefore lower priority than completion-to-task wake overhead. The first
perf attempt also exposed that the connection-scoped duplicate descriptor
doubles fd consumption: miniss reached `EMFILE` under perf's 1024 soft limit
at 800 connections, while the comparison runtimes did not. Those invalid
miniss runs are excluded above; removing the duplicate through a generation-
safe shard registry/fixed-file design remains a capacity obligation.

## Single owned stream descriptor

`AsyncTcpStream` now transfers its original socket into the existing Arc lease
instead of retaining both the original and a registered duplicate. All stream
I/O operations lease that one identity until terminal completion. A raw-fd
transfer duplicates only when an operation lease is still live.

The exact 1024-soft-limit perf scenario that previously failed with `EMFILE`
completed 800 connections without socket errors after the change (580,796 RPS,
7.80 ms p99 over five seconds). Three alternating pairs against commit
`3c17c08` showed throughput changes of +0.07%, +2.11%, and +1.57% (paired median
+1.57%). Overall medians were 557,616 vs 547,895 RPS; p99 medians were 7.41 vs
8.46 ms. This is primarily a descriptor-capacity/lifetime improvement, with a
small measured throughput benefit, not a major hot-path optimization.

## Unmeasured copy and owner-local fast paths (2026-07-12)

The current worktree contains several deliberately isolated changes whose
performance effect is not yet claimed because the Orb VM cannot complete
startup in the current environment:

* io_uring `read_rent` skips the synchronous read probe and submits socket
  `RECV` directly; the owner completion path delivers CQEs through a
  shard-local queue instead of cross-thread waker notification.
* owner completion batches reuse their `Vec` storage by swapping buffers;
  normal ready-queue turns skip the full task-arena overflow scan and only scan
  after a real bounded-queue overflow; owner-thread wakes skip `unpark`.
* the benchmark HTTP framer tracks a logical prefix and compacts only when
  append capacity is exhausted, avoiding a per-request `Vec::drain` memmove.

Local evidence is limited to lifecycle and framer tests. These changes must be
compared one at a time on Linux, with at least three alternating runs and the
following counters retained: RPS, p99, task-clock/request,
context-switch/request, `read`/request, `write`/request, `writev`/request, and
`io_uring_enter`/request. No throughput or tail-latency improvement should be
inferred until that A/B evidence exists.

Three live Orb samples were collected after accessing the manually started VM
through the privileged Orb socket. Current worktree, 4 workers, 800
connections, 8 wrk threads, 5 seconds:

| Run | Requests | RPS | p99 |
|---:|---:|---:|---:|
| 1 | 1,323,684 | 259,766 | 114.23 ms |
| 2 | 2,575,368 | 509,678 | 35.98 ms |
| 3 | 2,687,079 | 532,779 | 34.70 ms |

The median is 509,678 RPS and 35.98 ms p99, but run 1 is a retained noisy
outlier rather than silently discarded. A privileged perf sample recorded
25,564 `io_uring_enter` and 11,303 context-switch events over five seconds;
the raw perf file is authoritative. The derived diagnostics row was marked
`parse_error` while its tracepoint fields are being fixed, so its zero values
must not be used.

One same-host runtime comparison (same 4 workers, 800 connections, 8 wrk
threads, 5 seconds) measured miniss at 619,447 RPS / 34.36 ms p99, Monoio at
579,773 RPS / 28.95 ms p99, and Compio at 551,641 RPS / 24.93 ms p99. This is
one sample, not a new baseline, but it changes the optimization priority:
throughput is no longer the obvious primary gap; tail latency and scheduling
variance deserve the next isolated measurements.
