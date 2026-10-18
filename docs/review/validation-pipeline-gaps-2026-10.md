# Validation gaps and local correctness checks

## Why prior validation missed the mio/epoll baseline risks

The existing Linux validation requires kernel 6.12+ and therefore builds the
io_uring backend. macOS exercises the shared mio implementation through kqueue,
but not Linux epoll's platform-specific path. Passing those checks did not prove
the Linux epoll configuration compiled or behaved correctly. This is a coverage
gap, not evidence that epoll should match io_uring performance.

| Risk | Why prior checks did not expose it | Useful correctness check |
|---|---|---|
| Linux epoll fallback | No Linux epoll build/test leg was part of the normal validation; macOS kqueue does not compile Linux-specific epoll branches. | On Linux, explicitly compile and run relevant tests with `MINISS_IO_BACKEND=epoll` and an isolated Cargo target directory. This is for correctness only; no throughput or latency target is implied. |
| Positioned file I/O sharing an open-file-description offset | Existing file tests exercised ordinary writes/reads and offset cases serially; duplicated descriptors share the underlying file offset. | Add concurrent different-offset operations on one inode and assert exact contents on both io_uring and mio backends. |
| File jobs outliving mio shutdown/quiescence accounting | Tests did not hold a blocking file job in flight while cancellation or runtime shutdown checked pending work. | Use a deterministic blocked-job seam and assert shutdown does not claim quiescence before completion is published. |
| Mio token/slot growth over a long-lived runtime | Existing tests did not exercise enough sequential registrations to assert slots are reused or retained storage stays bounded. | Add a long-lived sequential-I/O test with an explicit retained-slot/capacity assertion. |
| UDP send registration error handling | Tests covered successful registration, not a forced `Poll::register` failure after pending state was created. | Inject failure at registration and assert pending state and descriptor lease are removed. |

These mio lifecycle risks remain separate from the nine findings fixed in this
review. The earlier checks covered their selected backends; they did not exercise
the unselected Linux epoll path or these unusual lifecycle transitions.

## Suggested validation workflow

These are reproducible local checks and future test recommendations. This review
does not add or enable GitHub Actions jobs.

1. Keep the normal io_uring and macOS/kqueue correctness checks. When validating
   the Linux fallback, run a Linux epoll build and relevant correctness tests,
   for example:

   ```sh
   MINISS_IO_BACKEND=epoll CARGO_TARGET_DIR=target/epoll cargo check --all-targets --all-features
   MINISS_IO_BACKEND=epoll CARGO_TARGET_DIR=target/epoll cargo test --all-features --lib --tests --examples -- --test-threads=1
   ```

   This is a compatibility check only, not a performance gate.
2. Keep named regressions for unrelated-wake readiness re-poll, bounded QUIC
   retirement capacity, sampler-start rollback, metrics success-then-failure,
   and a TLS-1.2-only peer. Network-facing test waits should remain bounded.
3. To check Prometheus compatibility locally, scrape the live metrics example
   and validate it with the pinned `promtool` using
   `bash scripts/check_prometheus_metrics.sh` (requires Docker).
4. `just docs-audit` locally checks README links and copied baseline rows against
   the canonical matrix. Prefer a single source of benchmark truth.
5. Use deterministic fault-injection seams for rare startup and registration
   failures. Do not make tests pass by skipping cases or increasing timeouts.

The epoll checks above establish only that the shared mio backend compiles and
passes correctness tests on Linux. No epoll performance claim is made. The four
mio risks in the table remain open; backend coverage alone does not fix or fully
test them.
