# Runtime Remediation Implementation Status

Verified on 2026-07-10. This is a review record, not a claim that the runtime
is production-safe. `docs/review/verified-remediation-plan.md` remains the
authority for scope and acceptance.

| Card | Recorded status | Verified status | Evidence / next action |
|---|---|---|---|
| rm-00 | GREEN | GREEN | Orb Linux: fmt, all-target check, strict clippy, and tests/examples all pass. Benchmark binaries are excluded from the test gate because `--all-targets` executes the intentionally long-running listener benchmark. |
| rm-01 through rm-05 | TODO | TODO | No corresponding commits or source fixes are present on `main` (`6361b4d`). |
| rm-06 | TODO on `main` | YELLOW candidate only | Commit `d49365c` exists only in a detached subagent worktree and is not merged. Its structural iovec fix needs Linux delayed-submit evidence and the required all-target gate before it can be accepted. |

## Review boundary

rm-00 gate repair is ready to amend into its existing one-card commit. The
worktree also contains an unrelated untracked `.codebase-memory/` directory;
it is preserved.
