# Runtime Remediation Implementation Status

Verified on 2026-07-10. This is a review record, not a claim that the runtime
is production-safe. `docs/review/verified-remediation-plan.md` remains the
authority for scope and acceptance.

| Card | Recorded status | Verified status | Evidence / next action |
|---|---|---|---|
| rm-00 | GREEN | GREEN | Orb Linux: fmt, all-target check, strict clippy, and tests/examples all pass. Benchmark binaries are excluded from the test gate because `--all-targets` executes the intentionally long-running listener benchmark. |
| rm-01 | YELLOW | YELLOW | Commit `128e5ca` contains scoped TLS. Follow-up closes review gap: context installation is unsafe crate-private, task setter crate-private, and timer/task/core nesting regression passes. Local check/clippy pass; Orb full gate pending. |
| rm-02 | GREEN | GREEN | Local and Orb Linux check/clippy pass. Drivers/state are local `Box` ownership, no unsafe Send/Sync impls; Runtime reuses one owned driver/timer pair across block_on. |
| rm-03 | TODO | TODO | No corresponding commits or source fixes on `main`. |
| rm-04 | TODO | TODO | No corresponding commits or source fixes on `main`. |
| rm-05 | YELLOW | YELLOW | Poll budgets and self-waker fairness test pass locally; ready-queue dedup awaits rm-04 safe waker. |
| rm-06 | GREEN | GREEN | Merged as `e28431f`. Orb Linux vectored tests cover 1–4/spilled metadata shapes plus delayed-submit kernel readv/writev under relocation pressure. |
| rm-07 | TODO | TODO | No corresponding commits or source fixes on `main`. |
| rm-08 | YELLOW | YELLOW | Commit `e3cc5db` containment path had missing backend coverage. Follow-up adds fallback file/TCP/readv/UDP tests and fixes empty-buffer `ReadVectored` capacity loss. macOS fallback tests pass 5/5; Orb full gate pending. |

## Review boundary

The worktree also contains an unrelated untracked `.codebase-memory/`
directory; it is preserved.
