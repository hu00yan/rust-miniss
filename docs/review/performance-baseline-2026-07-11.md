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
