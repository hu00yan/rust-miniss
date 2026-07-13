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
