# Runtime Remediation Implementation Status

Verified on 2026-07-11. Orb Ubuntu lib/integration tests (including 66 lib
tests), local fmt/check/clippy, and `just arch-audit` were rerun successfully.
This is a review record, not a claim that the runtime
is production-safe. `docs/review/verified-remediation-plan.md` remains the
authority for scope and acceptance.

| Card | Recorded status | Verified status | Evidence / next action |
|---|---|---|---|
| rm-00 | GREEN | GREEN | Orb Linux: fmt, all-target check, strict clippy, and tests/examples all pass. Benchmark binaries are excluded from the test gate because `--all-targets` executes the intentionally long-running listener benchmark. |
| rm-01 | GREEN | GREEN | Scoped closure-based TLS plus RAII restore covers panic/nesting; Orb Linux full test gate and strict check/clippy pass. |
| rm-02 | GREEN | GREEN | Local and Orb Linux check/clippy pass. Drivers/state are local `Box` ownership, no unsafe Send/Sync impls; Runtime reuses one owned driver/timer pair across block_on. |
| rm-03 | YELLOW | YELLOW | Commit `2a30856`: Executor/CpuCore live storage uses bounded generational `TaskArena`; external admission rejects stale/wrong-shard/out-of-capacity keys, runtime generations are per-runtime. `Runtime::try_spawn`/`try_spawn_detached` now expose arena admission failure without panic. Orb Linux: 66 lib + 22 multicore tests pass. Local `!Send` futures remain separate; full capacity/backpressure integration remains. |
| rm-04 | GREEN | GREEN | Commit `2a30856`: safe Arc wake state, bounded queue, dedup, cross-thread/late wake, sticky overflow marker, owner unpark, and arena-rescan retry tests pass; Orb Linux lib coverage passes. Loom remains a final rm-17 evidence item. |
| rm-05 | GREEN | GREEN | Configurable poll budgets, self-waker fairness, and bounded ready-queue recovery pass in the Orb Linux lib/multicore suites. |
| rm-06 | GREEN | GREEN | Merged as `e28431f`. Orb Linux vectored tests cover 1–4/spilled metadata shapes plus delayed-submit kernel readv/writev under relocation pressure. |
| rm-07 | YELLOW | YELLOW | Commits `4e25b39`, `a787a95`, `63a6c5b` retain dup-based `OwnedFd` leases for io_uring and fallback acceptance/file/readiness paths, with terminal cleanup and lease regressions. fd-reuse stress remains; accepted-result `OwnedFd` delivery is covered by rm-14. |
| rm-08 | GREEN | GREEN | Zeroed/validated fallback read paths and vectored-capacity fix; fallback regression suite passes on macOS and Orb full tests pass (Linux fallback cfg is not selected). |
| rm-09 | GREEN | GREEN | `IoProvider::submit` now returns `Result<IoToken, IoError>` across io_uring, fallback, net, QUIC, multicore, fs, tests, and examples. Descriptor/SQ/unsupported/registration failures reject before token exposure; `park` retains pending SQEs on real enter errors and treats `ETIME` as a normal timeout. Orb Linux: 66 lib, 22 multicore, and 8 runtime-regression tests pass; fallback admission/short-read regressions pass locally. |
| rm-10 | YELLOW | YELLOW | Commits `780c67b`/`681b8ad`: cancellation routes by full owner shard with backpressure errors; `JoinHandle::cancel` now sets a request, wakes the owner, and resolves only after owner poll acknowledgement. `CancelOutcome` and owner reply channel now expose Accepted/NotFound/AlreadyTerminal/Backpressured/RuntimeStopping (terminal accounting remains). Orb/local cancel regressions pass. |
| rm-11 | GREEN | GREEN | `CancellationToken` uses lost-wake-safe `AtomicWaker` registration/recheck and wakes on cancel; permanently-pending regression and Orb Linux lib/integration tests pass. |
| rm-12 | YELLOW | YELLOW | `join` now drains and aggregates all worker failures; `Drop` signals and joins every worker before eventfd ownership drops. Worker exit now also requires the owner backend's pending-operation/accept-result count to reach zero, preventing abandoned I/O from being reclaimed early. Deadline/escalation and explicit driver cancellation state machine remain. |
| rm-13 | YELLOW | YELLOW | Commits `2a30856`/`a2ccc71`: admission counter/close bit covers cross-core sends and retains a ticket through shard-local spawn queue/future lifetime; shutdown waits for in-flight admissions and post-cutoff spawn regression passes. Explicit owner acknowledgement enum remains. |
| rm-14 | YELLOW | YELLOW | Commit `17ff822` adds runtime io_uring accept-opcode probing; bounded result queues close overflow fds and terminal errors clear listener tokens. `CompletionKind::Accept` now owns an `OwnedFd` until `TcpStream` conversion, so abandoned queued results close safely. Kernel multishot-flag probing and listener-drop cancellation remain; Orb accept regression passes. |
| rm-15 | GREEN | GREEN | `spawn_local` accepts `!Send` futures on shard-local storage; startup enumerates allowed physical package/core IDs (including non-contiguous cpusets), affinity failure is explicit, and readiness is acknowledged only after owner-thread TLS/driver context installation. HTTP handlers stay local and fake `block_on_cpu` rejects explicitly. Orb Linux multicore suite: 22/22. |
| rm-16 | YELLOW | YELLOW | QUIC UDP socket/poller now bind to the first worker shard that polls them; cross-shard poll and non-owner token clearing are rejected/avoided. clippy/check pass. Signal lifecycle, blocking send, and global-runtime join remain. |
| rm-17 | YELLOW | YELLOW | Audit hard-fail checks and optional Miri/Loom recipes are present; `just arch-audit` exits 0 with unsafe justification output. Host stable toolchain lacks Miri and no Loom model targets exist yet. |

## Review boundary

The worktree also contains an unrelated untracked `.codebase-memory/`
directory; it is preserved.

The remaining YELLOW cards are intentional and recorded in
`docs/DECISION_LOG.md`; no deferred API or lifecycle design was silently
reclassified as complete.
