# Runtime Remediation Epic

**Authority:** `docs/review/verified-remediation-plan.md` (supersedes S0/S1 guidance in `runtime-audit.md`).

**Goal:** Remove safe-API paths to UB and restore lifecycle/ownership before any performance work.

**Rule:** one task card → one PR-sized change → **one commit**. Do not combine cards.

**Status legend:** `TODO` | `IN_PROGRESS` | `GREEN` | `YELLOW` | `RED`

## Task index (18)

| ID | Title | Batch | Depends on | Parallel wave |
|----|-------|-------|------------|---------------|
| [rm-00](rm-00-gate-all-targets.yaml) | Repair all-target compile gate | 0 | — | W0 (serial first) |
| [rm-01](rm-01-scoped-tls-context.yaml) | Scoped TLS / context guard | 1-A | rm-00 | W1 |
| [rm-02](rm-02-local-driver-ownership.yaml) | Local-only driver / !Send state | 1-A | rm-01 | W2 |
| [rm-03](rm-03-task-key-arena.yaml) | Generational TaskKey + arena | 1-B | rm-00 | W1 (// with rm-01 if careful) |
| [rm-04](rm-04-safe-waker.yaml) | Safe Arc/Wake waker + dedup | 1-B | rm-03 | W2 |
| [rm-05](rm-05-poll-budget.yaml) | Per-turn poll budget | 1-B | rm-04 | W3 |
| [rm-06](rm-06-stable-iovec.yaml) | Stable iovec / PendingOp !Clone | 1-C | rm-00 | W1 |
| [rm-07](rm-07-descriptor-lease.yaml) | Descriptor lease + abandon ≠ reclaim | 1-C | rm-06 | W2 |
| [rm-08](rm-08-buffer-init.yaml) | Initialized-buffer correctness | 1-D | rm-00 | W1 |
| [rm-09](rm-09-fallible-submit.yaml) | Fallible submit / no sentinel token | 2 | rm-07 | W3 |
| [rm-10](rm-10-cancel-routing.yaml) | Owner-acked task cancel | 2 | rm-03, rm-04 | W3 |
| [rm-11](rm-11-cancellation-token.yaml) | CancellationToken wake-safe | 2 | rm-04 | W3 |
| [rm-12](rm-12-shutdown-lifecycle.yaml) | Bounded shutdown state machine | 3 | rm-02, rm-07, rm-10 | W4 |
| [rm-13](rm-13-admission-linearize.yaml) | Admission linearization | 3 | rm-12 | W5 |
| [rm-14](rm-14-multishot-accept.yaml) | Multishot accept bounds/OwnedFd | 4 | rm-07, rm-09 | W4 |
| [rm-15](rm-15-affinity-local-spawn.yaml) | Affinity + local spawn + HTTP | 4 | rm-02, rm-05 | W4 |
| [rm-16](rm-16-quic-signal-global.yaml) | QUIC token + signal + global join | 2/3 | rm-07, rm-12 | W5 |
| [rm-17](rm-17-arch-audit-safety-tests.yaml) | Arch-audit hardening + Miri/Loom | 5 | rm-04, rm-08 | W1+ final (GREEN: Orb Miri pure targets + Loom models pass) |

## Recommended execution order

```text
W0:  rm-00
W1:  rm-01 ∥ rm-03 ∥ rm-06 ∥ rm-08 ∥ rm-17(script-only partial)
W2:  rm-02 → after rm-01
     rm-04 → after rm-03
     rm-07 → after rm-06
W3:  rm-05 → after rm-04
     rm-09 → after rm-07
     rm-10 → after rm-03+rm-04
     rm-11 → after rm-04
W4:  rm-12 → after rm-02+rm-07+rm-10
     rm-14 → after rm-07+rm-09
     rm-15 → after rm-02+rm-05
W5:  rm-13 → after rm-12
     rm-16 → after rm-07+rm-12
     rm-17 complete (Miri/Loom evidence)
```

**Practical note:** true multi-agent parallel is only safe when `allowed_files` do not overlap. Prefer single-agent sequential waves when in doubt.

## Explicit non-goals (remain out of scope for this remediation)

- Throughput optimization / monoio comparison claims
- SQPOLL / registered buffers / IO_LINK
- Treating `cargo check --lib` alone as closing an S0

## Commit policy

- Exactly **one git commit per task card** when that card reaches GREEN.
- Commit message references task id, e.g. `fix(rm-00): repair all-target IoToken test visibility`.
- RED → `git reset` of that card's work; record in `docs/FAILED_ATTEMPTS.md`.
