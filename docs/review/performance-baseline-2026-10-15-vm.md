# HTTP miniss A/B — 2026-10-15 (`main` = 7/15 code, vs `fix/io-backend-hygiene-and-macos-correctness`)

This answers one question: **did this branch regress or improve HTTP throughput
against the 2026-07-15 baseline?** It is a same-host, same-day, interleaved A/B
on the OrbStack Linux VM. It is not a runtime ranking.

The earlier `docs/review/performance-baseline-2026-10-15.md` is a different
measurement (1 core, inside a container, `main` vs branch). It does not use the
7/15 baseline and must not be read as a 7/15 comparison.

## Why an A/B instead of a straight comparison against 7/15

The recorded 7/15 numbers cannot be compared directly against a run made today:

- The 7/15 matrix ran on kernel `7.0.11-orbstack-00360-gc9bc4d96ac70`. This VM
  now runs `7.2.9-orbstack-00391-g2b298d05fbd4`.
- The 7/15 matrix ran with a five-runtime rotation; this run is miniss-only, so
  the per-scenario process warm-up sequence differs.
- The 7/15 numbers are themselves a single round. `README.md` already records
  that their verdict was "failed for stability": within-revision spread exceeded
  the between-revision difference.

So `main` — the exact 7/15 revision — was measured **today, on today's kernel**,
alternating with the branch. Anything the two arms share is environmental and
cancels; only the difference between arms is attributable to the code.

## Environment

- OrbStack Ubuntu 25.04 **VM** (not a container), aarch64, 8 vCPU.
- Kernel `7.2.9-orbstack-00391-g2b298d05fbd4`; `io_uring` on both arms
  (`build.rs` requires kernel 6.12+ and panics otherwise, so the `mio` backend is
  not even compiled on Linux).
- Host: MacBook Pro M1 Pro, 8 cores (6P + 2E).
- `HTTP_CORES=4`, `taskset` server CPUs `0-3`, client CPUs `4-7`, 8 wrk threads.
- Scenarios: `ka8/32/200/800`, `close8/32/800` (same shapes as the 7/15 matrix).
- No socket errors, no HTTP errors, no process leaks in any round.

## Arms

| Arm | Revision | Tree |
|---|---|---|
| `main` | `c4f46a4` (the 7/15 baseline revision) | separate worktree `rust-miniss-main` |
| branch | `4c7d963` + uncommitted working-tree changes | the working repository |

`examples/http_echo.rs` is byte-identical on both arms, so the measured
difference is confined to `src/`.

## Method

7 rounds, each round running both arms back to back with the arm order
alternating. Reported statistic: **the median of the per-scenario, per-round
paired ratio (branch / main)** — a paired statistic, because per-round ratios
swing far more than the median does.

Rounds 1–2 were taken while `Photos.app` was running background analysis on the
host (4–202 % CPU, host load average 21.97 on 8 cores). Those rounds are
**excluded** from the verdict and kept only to show the failure mode.

## Result — quiet host, rounds 3–7

| Scenario | main median RPS | branch median RPS | paired ratio (median) |
|---|---:|---:|---:|
| ka8 | 130,629 | 130,946 | 0.998 |
| ka32 | 388,943 | 371,758 | 0.985 |
| ka200 | 709,041 | 704,558 | 0.994 |
| ka800 | 685,992 | 678,308 | 0.979 |
| close8 | 135,808 | 127,850 | 0.993 |
| close32 | 394,341 | 386,990 | 0.997 |
| close800 | 692,917 | 697,265 | 0.999 |

**Median of the 35 paired ratios: 0.9937.** Every scenario median lies in
`0.979 – 0.999` (2.0 % spread).

p99 ratios, per scenario median: `0.869 – 1.140`, with no consistent direction.

### Individual rounds are still noisy

The per-scenario *round* ratios for the quiet rounds range from `0.689` (ka200,
r4) to `1.103` (ka32, r5). Contended rounds swing wider (`0.845` to `1.421`). A
single round therefore carries no verdict; a first, standalone branch-only round
taken before the A/B reported ka32 at 207,662 RPS (−48 %), which the paired data
shows to be pure noise.

## The gap to the recorded 7/15 numbers is environmental

`main` measured today is at or below the 7/15 record on most scenarios:

| Scenario | 7/15 record | `main` today | delta |
|---|---:|---:|---:|
| ka8 | 152,040 | 130,629 | −14.1 % |
| ka32 | 399,792 | 388,943 | −2.7 % |
| ka200 | 760,140 | 709,041 | −6.7 % |
| ka800 | 816,255 | 685,992 | −16.0 % |
| close8 | 134,749 | 135,808 | +0.8 % |
| close32 | 403,802 | 394,341 | −2.3 % |
| close800 | 848,422 | 692,917 | −18.3 % |

`main` *is* the 7/15 code, so this shortfall cannot be a code regression. At
ka800 the p50 is *better* today (≈390 us vs 415 us recorded) while throughput is
lower, which is the signature of a starved load generator: host contention
throttles `wrk` on CPUs 4–7, so the server is never driven to the throughput the
7/15 run reached.

## Verdict

- **No detectable throughput regression from this branch.** The paired median is
  0.9937, i.e. −0.6 %, inside the noise floor.
- **No detectable improvement either.** The branch's two new features (metrics
  aggregation, macOS-only flush group commit) are not on this path: `metrics` is
  off by default and the flush group commit lives in `src/io/mio_poll.rs`, which
  is not compiled on Linux.
- **Do not compare today's absolute numbers against the 7/15 table.** Any such
  comparison measures the kernel change plus the host, not the code.

## What this does not claim

- Not a runtime ranking: only miniss was measured.
- Not a same-kernel comparison against 7/15: the kernel differs, which is exactly
  why the `main` arm was re-measured.
- Not tail-latency evidence: p99 ratios span `0.869 – 1.140` across scenarios and
  no direction is consistent. The host cannot resolve p99 differences at this
  granularity.

## Artifacts

`bench/results/ab-20261015/` (gitignored): `main-r1..r7/`, `branch-r1..r7/`, each
with `matrix.csv`, `summary.csv`, `metadata.json`, wrk output and server logs.
`metadata.json` records `server_cpus=0-3`, `client_cpus=4-7`,
`affinity_mode=taskset`, `cores=4`, `threads=8` for every round.
