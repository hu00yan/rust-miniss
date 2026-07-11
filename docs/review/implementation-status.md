# Runtime Remediation Implementation Status

Verified on 2026-07-11. Orb Ubuntu lib/integration tests (including 73 lib
tests), local fmt/check/clippy, and `just arch-audit` were rerun successfully.
This is a review record, not a claim that the runtime
is production-safe. `docs/review/verified-remediation-plan.md` remains the
authority for scope and acceptance.

| Card | Recorded status | Verified status | Evidence / next action |
|---|---|---|---|
| rm-00 | GREEN | GREEN | Orb Linux: fmt, all-target check, strict clippy, and tests/examples all pass. Benchmark binaries are excluded from the test gate because `--all-targets` executes the intentionally long-running listener benchmark. |
| rm-01 | GREEN | GREEN | Scoped closure-based TLS plus RAII restore covers panic/nesting; Orb Linux full test gate and strict check/clippy pass. |
| rm-02 | GREEN | GREEN | Local and Orb Linux check/clippy pass. Drivers/state are local `Box` ownership, no unsafe Send/Sync impls; Runtime reuses one owned driver/timer pair across block_on. |
| rm-03 | GREEN | GREEN | `Executor`/`CpuCore` live storage uses bounded generational `TaskArena`; full keys reject stale/wrong-shard/out-of-capacity admission, runtime generations are per-runtime, and `try_spawn`/`try_spawn_detached` report capacity errors. Orb Linux: 73 lib + 22 multicore tests pass; bounded-capacity regression passes locally. |
| rm-04 | GREEN | GREEN | Commit `2a30856`: safe Arc wake state, bounded queue, dedup, cross-thread/late wake, sticky overflow marker, owner unpark, and arena-rescan retry tests pass; Orb Linux lib coverage passes. A production-linked Loom seam remains unverified. |
| rm-05 | GREEN | GREEN | Configurable poll budgets, self-waker fairness, and bounded ready-queue recovery pass in the Orb Linux lib/multicore suites. |
| rm-06 | GREEN | GREEN | Merged as `e28431f`. Orb Linux vectored tests cover 1–4/spilled metadata shapes plus delayed-submit kernel readv/writev under relocation pressure. |
| rm-07 | GREEN | GREEN | Dup-based `OwnedFd` leases are acquired before io_uring/fallback queueing and released only at terminal cleanup; user-future abandonment clears delivery only. Orb Linux fd-reuse stress proves a read remains on the original pipe after numeric fd reuse; fallback lease regression and rm-14 accepted-result ownership tests pass. |
| rm-08 | GREEN | GREEN | Zeroed/validated fallback read paths and vectored-capacity fix; fallback regression suite passes on macOS and Orb full tests pass (Linux fallback cfg is not selected). |
| rm-09 | GREEN | GREEN | `IoProvider::submit` now returns `Result<IoToken, IoError>` across io_uring, fallback, net, QUIC, multicore, fs, tests, and examples. Descriptor/SQ/unsupported/registration failures reject before token exposure; `park` retains pending SQEs on real enter errors and treats `ETIME` as a normal timeout. Orb Linux: 73 lib, 22 multicore, and 8 runtime-regression tests pass; fallback admission/short-read regressions pass locally. |
| rm-10 | GREEN | GREEN | Cancellation routes by full owner shard and generation; bounded owner reply exposes `CancelOutcome` (Accepted/NotFound/AlreadyTerminal/Backpressured/RuntimeStopping), with no silent send success. `JoinHandle::cancel` requests cancellation, wakes the owner, and resolves only after owner polling drops the task future. Orb/local cancel regressions pass. |
| rm-11 | GREEN | GREEN | `CancellationToken` uses lost-wake-safe `AtomicWaker` registration/recheck and wakes on cancel; permanently-pending regression and Orb Linux lib/integration tests pass. |
| rm-12 | GREEN | GREEN | `join` drains and aggregates all worker failures; `Drop` signals and joins every worker before eventfd ownership drops. Worker exit requires backend pending-operation/accept-result count zero, including wakeup-poll cancellation; coordinator publishes `Quiescing -> Cancelling -> DrainingIo -> Joined/Failed`; forced shutdown requests backend-wide cancellation and performs bounded owner-thread terminal drain. Orb shutdown-deadline and phase regressions pass. |
| rm-13 | GREEN | GREEN | Admission counter/close bit spans cross-core sends and shard-local spawn queue/future lifetime; all cross-core spawn and retry paths wait for owner `TaskAdmissionOutcome::Accepted` after arena insertion. Shutdown waits for in-flight admissions; Orb multicore 22/22 passes. |
| rm-14 | GREEN | GREEN | Runtime io_uring accept-opcode/kernel-version capability checks, bounded result queues, `OwnedFd` delivery, and single-shot sockaddr-retaining fallback are implemented. Listener drop requests owner-backend cancellation; cancellation drops queued OwnedFd results, overflow and sockaddr-parse paths close exactly once, canceled multishot CQEs are terminal, and live slot identities are cleared. Orb accept regressions pass. |
| rm-15 | GREEN | GREEN | `spawn_local` accepts `!Send` futures on shard-local storage; startup enumerates allowed physical package/core IDs (including non-contiguous cpusets), affinity failure is explicit, and readiness is acknowledged only after owner-thread TLS/driver context installation. HTTP handlers stay local and fake `block_on_cpu` rejects explicitly. Orb Linux multicore suite: 22/22. |
| rm-16 | GREEN | GREEN | QUIC UDP socket/poller bind to the first worker shard that polls them; cross-shard poll and non-owner token clearing are rejected/avoided. Signal shutdown is bounded/non-blocking with owner flags as lifecycle point. `join_global` and bounded `shutdown_global_with_timeout` now join the process-global runtime; global join regression passes. |
| rm-17 | YELLOW | YELLOW | Audit hard-fail checks and precise Miri/Linux recipes are present; `just test-fast` adds static locality constraints and two 64-case lifecycle models. The previous full-runtime Loom tests were invalid and removed; `just loom` now fails explicitly until three production synchronization seams exist. Backend syscalls remain outside Miri. |

## Review boundary

The worktree also contains an unrelated untracked `.codebase-memory/`
directory; it is preserved.

All remediation cards except rm-17 remain recorded GREEN against the evidence
above. rm-17 is YELLOW because production-linked Loom seams remain absent. The
runtime remains explicitly non-production-safe where the authority document
lists follow-up evidence; no deferred API or lifecycle design was silently
changed.
