# Performance stability matrix — 2026-07-13

This is a same-host diagnostic comparison, not a safety or production claim.
Orb Ubuntu reports Linux 7.0.11-orbstack, aarch64, 8 vCPUs. Every sample used
the same response body, 4 server workers, 800 connections, 8 wrk threads, and
10 seconds after a 3-second warmup. Runtime order was rotated between three
rounds to reduce order bias. Socket and HTTP errors abort a sample.

Raw logs are retained outside the repository at
`/tmp/miniss-stability-20260713` on Orb. The tracked harness is
`bench/scripts/bench_http.sh`; it records wrk, server, perf, and RSS outputs.

## Results

| Runtime | RPS samples | median RPS | min–max | p99 samples | median p99 |
|---|---:|---:|---:|---:|---:|
| miniss | 652,242 / 715,404 / 731,662 | **715,404** | 652,242–731,662 (CV 6.00%) | 22.91 / 19.86 / 30.03 ms | **22.91 ms** |
| monoio | 657,423 / 677,915 / 621,251 | 657,423 | 621,251–677,915 (CV 4.40%) | 12.90 / 32.47 / 27.24 ms | 27.24 ms |
| compio | 454,623 / 609,054 / 679,440 | 609,054 | 454,623–679,440 (CV 19.79%) | 284.82 / 35.72 / 20.21 ms | 35.72 ms |
| tokio | 605,334 / 547,034 / 594,190 | 594,190 | 547,034–605,334 (CV 5.32%) | 8.40 / 16.95 / 5.84 ms | 8.40 ms |

The current three-sample median puts miniss about 9% above Monoio, 17% above
Compio, and 20% above Tokio in throughput for this workload. The spread is too
large to call the ranking stable: Compio varied by roughly 33% and miniss by
roughly 11% from minimum to maximum. Tail latency is also workload- and run-
dependent; miniss did not have the lowest p99 even when it had the highest
throughput.

One visible source of variance is host scheduling/VM contention, not a hidden
benchmark retry: all samples ran on the same shared 8-vCPU Orb VM, and the
server and wrk client together consume all virtual CPUs. The first Compio
sample (454,623 RPS / 284.82 ms p99) is retained as evidence, not discarded.
The next baseline should pin or reserve CPUs for server and client separately
if the environment permits, then repeat at least five alternating rounds.

The post-warmup perf files show the io_uring path is not issuing one kernel
entry per request. For example, miniss recorded about 230k–245k
`io_uring_enter` calls for 6.6–7.4M requests (about 0.034–0.035 enters/request)
and task-clock around 21–23 seconds per 10-second sample. The current event set
does not include `recv`/`send`, so its zero `read`/`write`/`writev` rows do not
mean that network syscalls are absent. This does not by itself prove an
optimization opportunity; syscall counts must be compared with the same
counters from each competitor and with CPU pinning held constant.

## Functionality status

The local full test sweep passed (73 library tests plus the integration and
example targets). HTTP/1.1, Hyper HTTP/2, QUIC/HTTP-3, TCP, UDP, file I/O, and
Tokio/Futures compatibility tests passed on macOS. The HTTP/2 and QUIC tests
were also run manually on Orb Linux; the commands and target matrix are in
`docs/review/protocol-integration-2026-07-13.md`.

This is sufficient for the tested loopback and compatibility scenarios, but
not a claim of complete protocol coverage. Still-open obligations include
HTTP/2 flow-control/error matrices, QUIC migration/0-RTT/datagram/stream-reset
coverage, multicore QUIC affinity, UDP ownership stress, and a Linux CI gate.

The supported-platform contract is intentionally narrow: Linux requires
kernel 6.12+ for io_uring and macOS uses kqueue. There is no Linux epoll
fallback in the current build, and the documentation/build script now say so.

## Latency investigation log

The pinned six-round completion-drain A/B is retained at
`/tmp/miniss-drain-32-vs-256-20260713` on Orb. A post-park limit of 32 had a
slightly lower median p99 (10.39 ms versus 12.22 ms for 256), but 3.3% lower
median throughput, higher variation, and 31 ms outliers. It was therefore not
promoted to the production default; the source remains at the phase-specific
256/128/61 limits.

The static HTTP echo path submits a borrowed `WriteStatic` buffer directly to
io_uring and does not copy the response payload. This does not generalize to
the full HTTP response path: `HttpConnection::send_response` constructs header
storage and its vectored EAGAIN path may copy body/header buffers. That is a
separate, unverified latency hypothesis and must be measured with a workload
that actually exercises partial writes before changing it.

An additional six-sample Orb run compared `MINISS_URING_FLUSH_BATCH_SIZE=32`
with the default 61 in alternating order (the same 4/4 CPU split, 800
connections, 8 client threads, 5 seconds, and zero errors). The default had
median 793,116 RPS and median p99 10.73 ms; 32 had median 715,791 RPS and
median p99 10.31 ms. The small p99 difference is not worth the roughly 9.8%
throughput loss, so no flush-batch change is justified. Orb's perf tracepoint
permissions rejected `syscalls:sys_enter_io_uring_enter`; syscall counts remain
an explicit measurement gap rather than an inferred value.

A four-round source-level probe of `MAX_BUSY_ROUNDS=48` versus the existing
96 was also rejected. With the same pinned load, 48 reached a median 829,750
RPS and median p99 10.03 ms (one 16.77 ms outlier); 96 reached 855,202 RPS and
8.89 ms median p99. The constant is restored to 96.

Finally, a six-sample interleaved `MINISS_PARK_TIMEOUT_US=50` versus the
default 100 probe showed no latency win: 100 had median 855,914 RPS / 9.52 ms
p99, while 50 had 850,638 RPS / 9.68 ms p99. The timeout remains 100us.

An isolated high-level HTTP probe was intentionally discarded after exposing
a correctness gap: `HttpConnection::read_request` reads once into a fixed
buffer and parses the whole read. When keep-alive traffic coalesces a second
request into that read, the parser can reject the combined data and close the
connection. The probe produced only 18--399 requests/sec at 10--800
connections, so it is not a valid latency baseline. A keep-alive buffering
regression test now covers this case in `tests/integration_http.rs`, but the
high-level path still needs a purpose-built load client before tuning.

After the buffering fix, the frozen static runner was rerun for three pinned
rounds at 4 workers / 800 connections / 8 client threads. It completed with
zero errors and p99 values of 9.20, 8.90, and 9.79 ms (RPS 849,176 / 812,857 /
843,113). This is within the previously observed VM spread and is not claimed
as a latency improvement; the raw run is retained at
`/tmp/miniss-http-regression-20260713` on Orb.

io_uring feature probes were run with the same pinned load. In the initial
three-round screen, disabling COOP_TASKRUN reduced p99 slightly but lost about
8% throughput; SQPOLL lost about 6% throughput. A seven-round `SINGLE_ISSUER`
versus baseline set was initially promising, but a final six-round interleaved
default-versus-opt-in confirmation reversed the p99 ordering (9.23 ms versus
8.97 ms median) while throughput varied with the VM. None of these flags is
promoted to a new default. The source remains COOP_TASKRUN-on with
SINGLE_ISSUER/SQPOLL opt-in.

Because Orb's perf tracepoint is unavailable, a separate `strace -f -c` probe
measured syscall volume only (not latency). At 100 connections and 2 seconds,
`SINGLE_ISSUER` produced 65,662 `io_uring_enter` calls versus 74,262 for the
baseline (about 11.6% fewer), but strace changed scheduling and made its RPS
figures incomparable. The raw files are under
`/tmp/miniss-strace-flags-20260713` on Orb; this is supporting evidence for
the opt-in knob, not a production performance claim.

A follow-up response-path probe using explicit `Connection: close` reached about
28--36k requests/sec but had 82--98ms p99 under 800 concurrent connections.
Because every request includes connection setup/teardown, this is not
comparable with the frozen keep-alive static benchmark and is retained only as
a warning that high-level response latency needs a purpose-built client model.
