# Runtime Remediation Implementation Status

Verified on 2026-07-10. This is a review record, not a claim that the runtime
is production-safe. `docs/review/verified-remediation-plan.md` remains the
authority for scope and acceptance.

| Card | Recorded status | Verified status | Evidence / next action |
|---|---|---|---|
| rm-00 | GREEN | GREEN | Orb Linux: fmt, all-target check, strict clippy, and tests/examples all pass. Benchmark binaries are excluded from the test gate because `--all-targets` executes the intentionally long-running listener benchmark. |
| rm-01 | GREEN | GREEN | Scoped closure-based TLS plus RAII restore covers panic/nesting; Orb Linux full test gate and strict check/clippy pass. |
| rm-02 | GREEN | GREEN | Local and Orb Linux check/clippy pass. Drivers/state are local `Box` ownership, no unsafe Send/Sync impls; Runtime reuses one owned driver/timer pair across block_on. |
| rm-03 | YELLOW | YELLOW | TaskKey/TaskArena and stale-key validation committed; live storage still Vec-backed, arena integration remains. |
| rm-04 | YELLOW | YELLOW | Safe Arc wake state, bounded queue, dedup, cross-thread/late wake tests pass. Queue-full sticky overflow recovery remains. |
| rm-05 | YELLOW | YELLOW | Poll budgets and self-waker fairness test pass; rm-04 queue overflow recovery still gates final GREEN. |
| rm-06 | GREEN | GREEN | Merged as `e28431f`. Orb Linux vectored tests cover 1–4/spilled metadata shapes plus delayed-submit kernel readv/writev under relocation pressure. |
| rm-07 | YELLOW | YELLOW | Commits `4e25b39` and `a787a95` retain dup-based `OwnedFd` leases for io_uring and fallback acceptance/file jobs, with terminal cleanup and lease regressions. Fallback readiness re-arm still uses the original raw fd in one path; fd-reuse stress and full OwnedFd delivery remain. |
| rm-08 | GREEN | GREEN | Zeroed/validated fallback read paths and vectored-capacity fix; fallback regression suite passes on macOS and Orb full tests pass (Linux fallback cfg is not selected). |
| rm-09 | YELLOW | YELLOW | Commit `4e25b39` removes the `u64::MAX` sentinel path and delivers real terminal completion errors for admission/SQ failures; regression test `af5199b` passes locally. Public `submit` remains token-returning rather than `Result`, so literal API acceptance is deferred. |
| rm-10 | YELLOW | YELLOW | `cancel_task` now routes by full owner shard, propagates bounded-channel backpressure/disconnect, and rejects invalid owners; targeted regression passes. `JoinHandle::cancel` still lacks owner acknowledgement and future-drop completion. |
| rm-11 | YELLOW | YELLOW | `CancellationToken` now uses lost-wake-safe `AtomicWaker` registration/recheck and wakes on cancel; permanently-pending regression passes. Broader shutdown integration remains. |
| rm-12 | YELLOW | YELLOW | `join` now drains and aggregates all worker failures; `Drop` signals and joins every worker before eventfd ownership drops. Shutdown deadline/escalation and explicit I/O drain state machine remain. |
| rm-13 | YELLOW | YELLOW | Static review confirms the state-check/send race remains: no owner acknowledgement or admission cutoff exists. This is a design seam, so no speculative patch was applied. |
| rm-14 | YELLOW | YELLOW | io_uring multishot accept result queues are capped at 256; overflow accepted fds are closed, and terminal errors clear listener tokens. Capability probing and `OwnedFd` delivery remain. |
| rm-15 | YELLOW | YELLOW | `spawn_local` now accepts `!Send` futures, stores them in shard-local storage, and has an `Rc` regression test; check/clippy and multicore tests pass. `block_on_cpu`, physical-core affinity, readiness ordering, and HTTP keep-local path remain. |
| rm-16 | YELLOW | YELLOW | QUIC UDP socket/poller now bind to the first worker shard that polls them; cross-shard poll and non-owner token clearing are rejected/avoided. clippy/check pass. Signal lifecycle, blocking send, and global-runtime join remain. |
| rm-17 | YELLOW | YELLOW | Audit now checks escaped raw-Rc/TLS/unsafe-backend patterns and adds optional Miri/Loom recipes; cfg(test) runtime calls are filtered. One startup-only global-runtime construction remains reported, plus existing unsafe justifications. |

## Review boundary

The worktree also contains an unrelated untracked `.codebase-memory/`
directory; it is preserved.
