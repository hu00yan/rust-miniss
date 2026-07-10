# Task-card self-review (pre-execution)

Reviewed against `docs/review/verified-remediation-plan.md` and repo gates.

## Verdict

**Accept with notes.** 18 cards, coverage of S0/S1 is complete enough to start. No card redesign required before `rm-00`.

## Commit policy (confirmed)

| Rule | Decision |
|------|----------|
| One card → one commit | **Yes** |
| Commit only on GREEN | Yes (`just check` / card test_commands as applicable) |
| RED | Discard card work; log `docs/FAILED_ATTEMPTS.md` |
| Do not batch multiple cards into one commit | Hard rule |

So **18 successful cards ⇒ 18 commits** (plus this epic scaffolding may be commit 0 if you want cards tracked in git before code).

## Coverage check vs verified plan

| Plan item | Card | OK? |
|-----------|------|-----|
| S0-1 waker | rm-04 | Yes |
| S0-2 TLS | rm-01 | Yes |
| S0-3 driver Send | rm-02 | Yes |
| S0-4 iovec | rm-06 | Yes |
| S0-5 fd lease | rm-07 | Yes |
| S0-6 buffer init | rm-08 | Yes |
| S1-1 fallible submit | rm-09 | Yes |
| S1-2 cancel | rm-10 | Yes |
| S1-3 shutdown | rm-12 | Yes (large) |
| S1-4 fairness | rm-05 | Yes |
| S1-5 TaskKey | rm-03 | Yes |
| S1-6 admission | rm-13 | Yes |
| S1-7 multishot | rm-14 | Yes |
| S1-8 affinity/HTTP | rm-15 | Yes |
| S1-9 QUIC/signal/global | rm-16 | Yes |
| Arch audit gap | rm-17 | Yes |
| Gate broken | rm-00 | Yes |

### Intentionally deferred (not S0)

- Eventfd slab leak / IoToken monotonic growth (audit S2) — fold into rm-09/rm-14 if cheap, else follow-up epic
- Benchmark baselines (plan Batch 5 perf half) — **after** all GREEN; no card (correct)
- Full `RuntimeOwner` type rename — may land as part of rm-12 without separate card

## Risks / problems found

1. **rm-12 is the fattest card**  
   Shutdown + startup rollback + driver cancel interface. If it blows the diff budget, split at execution time into `rm-12a` startup/join and `rm-12b` drain — only if RED/YELLOW, not pre-split (keeps count ≤18).

2. **Parallel file conflicts**  
   W1 claims `rm-01 ∥ rm-03 ∥ rm-06 ∥ rm-08` but:
   - rm-01 and rm-03 both touch `multicore.rs` / `executor.rs` → **do not parallel in one worktree**
   - rm-06 and rm-07 serial on `uring.rs` (already sequenced)
   - True parallel only: **rm-06 ∥ rm-08 ∥ (rm-17 script-only)** after rm-00; run **rm-01 then rm-03** (or reverse) serially on same agent

3. **rm-03 before rm-04 is mandatory**  
   Card order correct; do not “quick fix waker” without TaskKey.

4. **Linux evidence**  
   Several acceptances need Linux. On macOS: implement structure + `cfg` tests; mark Linux stress as required evidence before closing S0 in DECISION_LOG — do not claim GREEN production-safe on Darwin alone.

5. **`tests/**` in allowed_files**  
   Broad on purpose for regression tests; `just diff-audit` may need path patterns — if audit is strict, narrow per card when coding.

6. **Public API breakage**  
   rm-02/rm-04/rm-09/rm-10 will break callers. Acceptable pre-1.0; document in each commit body.

## Parallel plan (revised, safer)

```text
Serial spine (single agent default):
  rm-00 → rm-01 → rm-02 → rm-03 → rm-04 → rm-05
                ↘ rm-06 → rm-07 → rm-09 → rm-14
                ↘ rm-08
       after rm-04: rm-10, rm-11
       after rm-02+rm-07+rm-10: rm-12 → rm-13
       after rm-02+rm-05: rm-15
       after rm-12: rm-16
       continuous: rm-17 (script early; Miri final)

Optional 2-agent parallel after rm-00:
  Agent A: rm-01 → rm-02 → rm-03 → rm-04 → rm-05 → rm-10 → rm-11
  Agent B: rm-06 → rm-07 → rm-08 → rm-09 → rm-14
  Merge barrier before rm-12
```

## Mood note (process, not cards)

Last year: incomplete + pseudo-sync risk.  
This year (June): S0 UB from perf rewrite.  
Cards assume **high-spec first**, then **dev executes one card/one commit** — matches the sol design+review / luna-dev split.

## Ready to execute?

**Yes — start `rm-00`.**
