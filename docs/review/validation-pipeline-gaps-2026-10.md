# Validation gaps and local correctness checks

## Why prior validation missed the mio/epoll baseline risks

The existing Linux validation requires kernel 6.12+ and therefore builds the
io_uring backend. macOS exercises the shared mio implementation through kqueue,
but not Linux epoll's platform-specific path. Passing those checks did not prove
the Linux epoll configuration compiled or behaved correctly. This is a coverage
gap, not evidence that epoll should match io_uring performance.

| Risk | Why prior checks did not expose it | Useful correctness check | Status in this change |
|---|---|---|---|
| Linux epoll fallback | No Linux epoll build/test leg was part of the normal validation; macOS kqueue does not compile Linux-specific epoll branches. | On Linux, run `MINISS_IO_BACKEND=epoll CARGO_TARGET_DIR="$HOME/miniss-target" just check`. Correctness only; no throughput or latency target. | OrbStack Ubuntu `just check` passed on the existing `/mnt/mac/Users/huyan00/mycode/rust-miniss` mount; no source copy, CI job, or benchmark. |
| Positioned file I/O sharing an open-file-description offset | Existing file tests exercised ordinary writes/reads and offset cases serially; duplicated descriptors share the underlying file offset. | Concurrent different-offset operations on one inode, exact content assertions, and check that the caller's shared offset stays unchanged. | Fixed with `FileExt::read_at`/`write_at` in the pool and inline paths; `pool_and_inline_file_io_preserve_the_shared_file_offset` passes. |
| File jobs outliving mio shutdown/quiescence accounting | Tests did not hold a blocking file job in flight while cancellation or runtime shutdown checked pending work. | Gate a worker job; assert pending stays nonzero through publication and until owner drain, and descriptor lifetime survives backend drop. | Fixed with shard-local in-flight completion accounting and shared `DescriptorLease`s; `blocking_file_and_flush_work_count_until_owner_drains_completion` and `file_worker_keeps_its_descriptor_lease_if_backend_drops_early` pass. |
| Mio token/slot growth over a long-lived runtime | Existing tests did not exercise enough sequential registrations to assert slots are reused or retained storage stays bounded. | Repeated sequential cancellation/completion, assert retained slot vectors stay bounded, and deliver an old harvested event after slot reuse. | Fixed with reusable slots plus monotonically unique mio tokens; `operation_slots_are_reused_and_stale_harvested_events_are_ignored` passes. |
| UDP send registration error handling | Tests covered successful registration, not a forced `Poll::register` failure after pending state was created. | Inject send and receive registration failures; assert operation state, receive buffer and lease are cleared and the slot is reusable. | Fixed: send registration failures return `IoError`; both UDP send and receive failure regressions pass. |

The four mio lifecycle risks are closed by this change. Their earlier absence was a
validation-coverage gap, not evidence that the mio backend should match io_uring
performance.

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

The epoll check establishes that the shared mio backend compiles and passes its
correctness suite on Linux. No epoll performance claim is made. The Linux run is
local-only; no GitHub Actions workflow or performance gate was added. The
Prometheus formatter remains an optional integration path over the provider-neutral
runtime snapshot API; see [`docs/METRICS.md`](../METRICS.md).
