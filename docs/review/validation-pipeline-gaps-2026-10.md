# Validation gaps for the July-to-October review

## Why prior validation missed the fallback-backend risks

The prior checks established that the default Linux artifact worked on the current
OrbStack kernel and that the macOS kqueue build passed. Those are useful but do
not exercise every implementation path:

| Risk | Why the earlier pipeline did not expose it | Gate that prevents recurrence |
|---|---|---|
| Linux epoll fallback behavior | The Linux CI job requires kernel 6.12+ and therefore builds/runs io_uring. macOS compiles the shared mio source as kqueue, not as Linux epoll. A successful Linux io_uring job did not compile or execute the fallback cfgs. | A dedicated Linux job sets `MINISS_IO_BACKEND=epoll`, uses an isolated Cargo target directory, and runs clippy, all-target checks, and the feature-complete tests. |
| Positioned file I/O sharing an open-file-description offset | Existing file tests exercised ordinary writes/reads and offset cases serially. They did not run concurrent positioned operations against the same duplicated descriptor; `dup` shares the file offset. The io_uring path also avoids the mio thread-pool implementation. | Add a same-inode concurrent-offset regression on both io_uring and epoll, and include the epoll job in required CI. |
| File jobs outliving mio shutdown/quiescence accounting | The ordinary suite did not hold a blocking file job in flight while cancellation or runtime shutdown checked pending work. The accepted-operation count alone did not describe work already handed to the thread pool. | Add a deterministic blocking-job test seam; assert shutdown does not claim quiescence until the handed-off work publishes completion. Run it on epoll and kqueue. |
| Mio token/slot growth over a long-lived runtime | Tests checked operation results and leaks, but did not perform enough sequential registrations to assert slot storage returns to a bounded/reusable state. | Add a long-lived sequential-I/O regression with an explicit retained-slot/capacity assertion, and run it in the epoll and kqueue suites. |
| UDP send registration error handling | Normal tests exercised successful registration; they did not force `Poll::register` to fail after the operation entered the pending table. | Add fault injection at the registration boundary and assert the operation and lease are removed on error. |

The four mio risks above are backend/lifecycle coverage gaps, not evidence that the
io_uring job was invalid. The previous validation checked the backend it built;
it did not prove the unselected epoll path or unusual failure transitions.

## Pipeline changes for this review

1. Keep separate io_uring and macOS/kqueue jobs, and add the explicit Linux epoll
   job described above so every backend is compiled and run in CI.
2. Put the reported state transitions into named regression tests: unrelated-wake
   readiness re-poll, bounded-mailbox pressure, sampler-start failure rollback,
   metrics success-then-failure, and a TLS-1.2-only peer. Network-facing client
   waits must have a bounded deadline rather than relying only on server timeouts.
3. Scrape the actual metrics example in CI and pass its response to a pinned
   Prometheus `promtool`; validating Rust strings alone cannot catch invalid text
   exposition names or types.
4. Check README's copied performance rows against the canonical dated matrix and
   reject broken local README links. Prefer a single source of benchmark truth.
5. Retain job-level bounds and use deterministic fault-injection seams for rare
   startup and registration failures; do not make tests pass by skipping cases or
   increasing timeouts.

The CI additions implement items 1, 3, and 4; the issue-specific regressions
implement item 2, and the new jobs have explicit job-level time bounds from item
5. The sampler-start failure injection also covers one of item 5's rare startup
transitions. The four mio risks in the table remain separate follow-up work: the
epoll job increases backend coverage, but no dedicated regression for those
specific behaviors has been added here, so this change does not claim they are
fixed.
