# Runtime Remediation Implementation Status

Verified on 2026-07-11. This is a review record, not a claim that the runtime
is production-safe. `docs/review/verified-remediation-plan.md` remains the
authority for scope and acceptance.

| Card | Recorded status | Verified status | Evidence / next action |
|---|---|---|---|
| rm-00 | GREEN | GREEN | Orb Linux: fmt, all-target check, strict clippy, and tests/examples all pass. Benchmark binaries are excluded from the test gate because `--all-targets` executes the intentionally long-running listener benchmark. |
| rm-01 | GREEN | GREEN | Scoped closure-based TLS plus RAII restore covers panic/nesting; Orb Linux full test gate and strict check/clippy pass. |
| rm-02 | GREEN | GREEN | Local and Orb Linux check/clippy pass. Drivers/state are local `Box` ownership, no unsafe Send/Sync impls; Runtime reuses one owned driver/timer pair across block_on. |
| rm-03 | YELLOW | YELLOW | Commit `2a30856`: Executor/CpuCore live storage uses bounded generational `TaskArena`; external admission rejects stale/wrong-shard/out-of-capacity keys, runtime generations are per-runtime. Orb Linux: 60 lib + 22 multicore tests pass. Local `!Send` futures remain separate; capacity exhaustion still needs fallible public spawn. |
| rm-04 | YELLOW | YELLOW | Commit `2a30856`: safe Arc wake state, bounded queue, dedup, cross-thread/late wake, sticky overflow marker, owner unpark, and arena-rescan retry tests pass; Orb Linux lib coverage passes. Loom proof and scan-cost evidence remain. |
| rm-05 | YELLOW | YELLOW | Poll budgets and self-waker fairness test pass; rm-04 queue overflow recovery still gates final GREEN. |
| rm-06 | GREEN | GREEN | Merged as `e28431f`. Orb Linux vectored tests cover 1–4/spilled metadata shapes plus delayed-submit kernel readv/writev under relocation pressure. |
| rm-07 | YELLOW | YELLOW | Commits `4e25b39`, `a787a95`, `63a6c5b` retain dup-based `OwnedFd` leases for io_uring and fallback acceptance/file/readiness paths, with terminal cleanup and lease regressions. fd-reuse stress and full `OwnedFd` accepted-result delivery remain. |
| rm-08 | GREEN | GREEN | Zeroed/validated fallback read paths and vectored-capacity fix; fallback regression suite passes on macOS and Orb full tests pass (Linux fallback cfg is not selected). |
| rm-09 | YELLOW | YELLOW | Commits `4e25b39`/`0037af1` remove sentinel admission paths and retain pending SQEs after `park` enter failure; Orb Linux terminal-failure regression passes. Public `submit` remains token-returning rather than `Result`, requiring API-wide migration before GREEN. |
| rm-10 | YELLOW | YELLOW | Commits `780c67b`/`681b8ad`: cancellation routes by full owner shard with backpressure errors; `JoinHandle::cancel` now sets a request, wakes the owner, and resolves only after owner poll acknowledgement. Full runtime cancel routing/terminal outcomes remain. |
| rm-11 | GREEN | GREEN | `CancellationToken` uses lost-wake-safe `AtomicWaker` registration/recheck and wakes on cancel; permanently-pending regression and Orb Linux lib/integration tests pass. |
| rm-12 | YELLOW | YELLOW | `join` now drains and aggregates all worker failures; `Drop` signals and joins every worker before eventfd ownership drops. Shutdown deadline/escalation and explicit I/O drain state machine remain. |
| rm-13 | YELLOW | YELLOW | Admission counter/close bit now covers cross-core bounded sends and shutdown waits for in-flight admissions; post-shutdown spawn regression passes in the Orb Linux multicore suite. Local-spawn queue ownership and explicit owner acknowledgement remain. |
| rm-14 | YELLOW | YELLOW | Commit `17ff822` adds runtime io_uring accept-opcode probing; bounded result queues close overflow fds and terminal errors clear listener tokens. Kernel multishot-flag probing and `OwnedFd` delivery remain. |
| rm-15 | YELLOW | YELLOW | Commits `c9eec36`/`8c467f1`: `spawn_local` accepts `!Send` futures on shard-local storage; default worker count uses physical CPUs, affinity failure is explicit, and readiness waits for initial bind. `block_on_cpu`, non-contiguous CPU enumeration, and HTTP keep-local path remain. |
| rm-16 | YELLOW | YELLOW | QUIC UDP socket/poller now bind to the first worker shard that polls them; cross-shard poll and non-owner token clearing are rejected/avoided. clippy/check pass. Signal lifecycle, blocking send, and global-runtime join remain. |
| rm-17 | YELLOW | YELLOW | Audit now checks escaped raw-Rc/TLS/unsafe-backend patterns and adds optional Miri/Loom recipes; cfg(test) runtime calls are filtered. One startup-only global-runtime construction remains reported, plus existing unsafe justifications. |

## Review boundary

The worktree also contains an unrelated untracked `.codebase-memory/`
directory; it is preserved.
