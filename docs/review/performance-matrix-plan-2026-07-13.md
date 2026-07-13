# Performance matrix and automatic verdicts

The current same-host `wrk` run is useful for throughput smoke tests, but the
client and server compete for the same CPUs. It must not be the only source of
tail-latency conclusions.

## Two separate measurements

1. **Primary comparison (isolated client):** run the server on the benchmark
   host with `S = 1, 2, 4, 8, 10` worker cores and run the load generator on a
   second host/Orb VM. Pin server CPUs and client CPUs explicitly. For each
   `S`, run client threads `1, 2, 4`, connections `8, 32, 200, 800`, and at
   least five alternating rounds per runtime.
2. **Same-host diagnosis:** pin the server to CPUs `0..S-1` and the client to
   the remaining CPUs. Keep this as a contention diagnostic, not as the main
   runtime ranking. `S=10` is invalid on a 10-core single host unless the
   client is remote.

For closed-loop maximum throughput use `wrk`. For p99 stability use `wrk2`
with a fixed request rate when available; a client CPU limit must be reported
instead of being mistaken for server behavior. HTTP/2 requires `h2load`, and
QUIC/H3 requires a dedicated H3 client; HTTP/1 `wrk` numbers do not cover those
protocols.

## Machine-verifiable case rules

Each case must fail, rather than require manual inspection, when any of these
conditions occurs:

- readiness timeout, non-zero process exit, leaked server/client process, or
  missing summary row;
- any wrk socket error, non-2xx response, or zero completed requests;
- missing host/kernel/backend/core-placement metadata;
- throughput coefficient of variation above 5% or p99 median absolute
  deviation above 10% (mark the environment unstable and repeat, never delete
  the outlier);
- compared with a locked baseline, throughput regression over 5% or p99
  regression over 10%.

Record RPS, p50/p99/p99.9, errors, server/client CPU, task-clock, context
switches, CPU migrations, RSS, `io_uring_enter` per request, and VM steal/load
signals. The existing `bench_http.sh` remains the per-case runner; the matrix
driver should add CPU pinning, client placement, JSON verdicts, and explicit
skip/failure reasons.

## Regression-test rule

Lifecycle tests must be bounded and run by default when the backend supports
them. `multicore_spawned_async_file_future_completes` now runs on both Linux
io_uring and macOS kqueue; its one-second completion and shutdown bounds remain
part of the regression contract. The ten-minute `stress_timer_io` test stays
opt-in and must never be pulled into the normal test gate.
