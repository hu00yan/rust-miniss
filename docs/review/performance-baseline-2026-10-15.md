# HTTP smoke A/B — 2026-10-15 (`main` vs `fix/io-backend-hygiene-and-macos-correctness`)

This is a miniss-only sandbox smoke comparison, not a runtime ranking and not a
speedup claim.

Environment and workload:

- OrbStack Ubuntu container, Linux `7.2.9-orbstack-00391-g2b298d05fbd4`, 8 vCPU.
- Both revisions select `io_uring` (the `build.rs` kernel probe sees 6.12+), so
  the backend is identical on both arms.
- `HTTP_CORES=1`, 1 wrk thread, 32 keep-alive connections, 5 seconds, no
  `taskset` placement, runs sequential in one container.

| Round | Revision | Requests/s | p50 | p99 | socket errors | leaks |
|---|---|---:|---:|---:|---:|---:|
| 1 | branch | 277,541.27 | 88 us | 617 us | 0 | 0 |
| 2 | branch | 298,643.96 | 81 us | 240 us | 0 | 0 |
| 3 | branch | 311,675.91 | 77 us | 181 us | 0 | 0 |
| 1 | `main` | 294,416.37 | 83 us | 236 us | 0 | 0 |
| 2 | `main` | 293,699.41 | 84 us | 192 us | 0 | 0 |
| 3 | `main` | 288,166.33 | 84 us | 1.02 ms | 0 | 0 |

Medians: branch 298,643.96 RPS / p99 240 us; `main` 293,699.41 RPS / p99 236 us.

Verdict: no regression from this branch in this environment — median throughput
is +1.7 % and median p99 is unchanged. The spread *within* one revision
(277k–312k RPS, p99 181 us–1.02 ms) exceeds the difference between revisions, so
neither arm supports a throughput or tail-latency claim from a single pair of
runs. `bench/scripts/compare_http_baseline.pl` run on the best branch round
against the worst `main` round reports `passed: true` (throughput ratio 1.08,
p99 ratio 0.18); that pass is a property of which outliers were picked, not
evidence of a speedup.

Raw artifacts (`summary.csv`, `metadata.json`, wrk/server logs) were written
under the container paths `/work/.scratch/ab-head-*/` and
`/work-main/.scratch/ab-main-*/` and are removed with the container; the numbers
above are the retained record.
