# Runtime Remediation Implementation Status

Verified on 2026-07-10. This is a review record, not a claim that the runtime
is production-safe. `docs/review/verified-remediation-plan.md` remains the
authority for scope and acceptance.

| Card | Recorded status | Verified status | Evidence / next action |
|---|---|---|---|
| rm-00 | GREEN | GREEN | Orb Linux: fmt, all-target check, strict clippy, and tests/examples all pass. Benchmark binaries are excluded from the test gate because `--all-targets` executes the intentionally long-running listener benchmark. |
| rm-01 | YELLOW | YELLOW | Commit `128e5ca` contains scoped TLS. Follow-up closes review gap: context installation is unsafe crate-private, task setter crate-private, and timer/task/core nesting regression passes. Local check/clippy pass; Orb full gate pending. |
| rm-02 | GREEN | GREEN | Local and Orb Linux check/clippy pass. Drivers/state are local `Box` ownership, no unsafe Send/Sync impls; Runtime reuses one owned driver/timer pair across block_on. |
| rm-03 | YELLOW | YELLOW | TaskKey/TaskArena and stale-key validation committed; live storage still Vec-backed, arena integration remains. |
| rm-04 | YELLOW | YELLOW | Safe Arc wake state, bounded queue, dedup, cross-thread/late wake tests pass. Queue-full sticky overflow recovery remains. |
| rm-05 | YELLOW | YELLOW | Poll budgets and self-waker fairness test pass; rm-04 queue overflow recovery still gates final GREEN. |
| rm-06 | GREEN | GREEN | Merged as `e28431f`. Orb Linux vectored tests cover 1–4/spilled metadata shapes plus delayed-submit kernel readv/writev under relocation pressure. |
| rm-07 | YELLOW | YELLOW | Commit `4e25b39` acquires a dup-based `OwnedFd` lease before io_uring SQE construction, keeps it by slab slot, and releases it on terminal CQE/SQ-full rejection. Lease lifetime regression passes. Epoll/kqueue acceptance-time lease and fd-reuse stress remain. |
| rm-08 | YELLOW | YELLOW | Commit `e3cc5db` containment path had missing backend coverage. Follow-up adds fallback file/TCP/readv/UDP tests and fixes empty-buffer `ReadVectored` capacity loss. macOS fallback tests pass 5/5; Orb full gate pending. |
| rm-09 | YELLOW | YELLOW | Commit `4e25b39` removes the `u64::MAX` sentinel path and delivers real terminal completion errors for admission/SQ failures; regression test `af5199b` passes locally. Public `submit` remains token-returning rather than `Result`, so literal API acceptance is deferred. |
| rm-10 | YELLOW | YELLOW | `cancel_task` now routes by full owner shard, propagates bounded-channel backpressure/disconnect, and rejects invalid owners; targeted regression passes. `JoinHandle::cancel` still lacks owner acknowledgement and future-drop completion. |

## Review boundary

The worktree also contains an unrelated untracked `.codebase-memory/`
directory; it is preserved.
