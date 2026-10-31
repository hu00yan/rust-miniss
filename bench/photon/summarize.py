#!/usr/bin/env python3
"""summarize.py — medians + percentage deltas for bench_photon_matrix.sh results.csv.

Usage: summarize.py <results.csv> [--photon photon-uring-sock]

Reads the CSV produced by bench_photon_matrix.sh (columns:
cores,server,scenario,round,rps,p50,p75,p90,p99,rss_kib,loader,order_note,notes)
and prints, per (cores, scenario), the median rps/p50/p99 per server, then
delta tables of miniss-framer vs photon and miniss-hyper-h1 vs photon.
"""
import argparse
import csv
import re
import statistics
import sys

UNIT_US = {
    "ns": 1e-3,
    "us": 1.0,
    "µs": 1.0,
    "ms": 1e3,
    "s": 1e6,
    "m": 6e7,
}


def lat_us(s):
    """wrk latency string (e.g. 105us, 4.64ms) -> float microseconds."""
    if not s:
        return None
    m = re.match(r"^\s*([0-9.]+)\s*([a-zµ]+)\s*$", s.strip())
    if not m:
        return None
    val, unit = float(m.group(1)), m.group(2)
    if unit not in UNIT_US:
        return None
    return val * UNIT_US[unit]


def fmt_us(x):
    if x is None:
        return "-"
    if x >= 1000:
        return f"{x/1000:.2f}ms"
    return f"{x:.0f}us"


def median(vals):
    vals = [v for v in vals if v is not None and v != ""]
    if not vals:
        return None
    return statistics.median(vals)


def load(path):
    rows = []
    with open(path) as f:
        for row in csv.DictReader(f):
            try:
                row["rps_f"] = float(row["rps"])
            except (ValueError, KeyError):
                row["rps_f"] = None
            row["p50_us"] = lat_us(row.get("p50", ""))
            row["p99_us"] = lat_us(row.get("p99", ""))
            rows.append(row)
    return rows


def agg(rows):
    """(cores, scenario, server) -> dict of medians."""
    groups = {}
    for r in rows:
        key = (r["cores"], r["scenario"], r["server"])
        groups.setdefault(key, []).append(r)
    out = {}
    for key, rs in groups.items():
        out[key] = {
            "rounds": len(rs),
            "rps": median([r["rps_f"] for r in rs]),
            "p50": median([r["p50_us"] for r in rs]),
            "p99": median([r["p99_us"] for r in rs]),
            "rss": median([float(r["rss_kib"]) if r["rss_kib"] else None for r in rs]),
        }
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("csv")
    ap.add_argument("--photon", default="photon-uring-sock",
                    help="photon server tag used as the delta baseline")
    ap.add_argument("--compare", default="miniss-framer,miniss-hyper-h1",
                    help="comma list of servers to compare against photon")
    args = ap.parse_args()

    rows = load(args.csv)
    agg_data = agg(rows)
    if not agg_data:
        sys.exit("no rows parsed")

    core_ids = sorted({k[0] for k in agg_data}, key=lambda x: int(x))
    scenario_ids = sorted({k[1] for k in agg_data})
    servers = sorted({k[2] for k in agg_data})

    print("## 结果表(3 轮中位数)\n")
    for cores in core_ids:
        print(f"### cores={cores}\n")
        hdr = "| scenario | " + " | ".join(servers) + " |"
        print(hdr)
        print("|---" * (len(servers) + 1) + "|")
        for sc in scenario_ids:
            cells = []
            for sv in servers:
                d = agg_data.get((cores, sc, sv))
                if not d or d["rps"] is None:
                    cells.append("-")
                elif d["p50"] is not None:
                    cells.append(f"{d['rps']:,.0f} / {fmt_us(d['p50'])} / {fmt_us(d['p99'])}")
                else:
                    cells.append(f"{d['rps']:,.0f}")
            print(f"| {sc} | " + " | ".join(cells) + " |")
        print()

    print("## 百分比结论(vs %s)\n" % args.photon)
    targets = [t.strip() for t in args.compare.split(",") if t.strip()]
    for cores in core_ids:
        for sc in ("ka8", "ka32", "close32"):
            base = agg_data.get((cores, sc, args.photon))
            if not base or base["rps"] is None:
                continue
            parts = []
            for t in targets:
                d = agg_data.get((cores, sc, t))
                if d and d["rps"]:
                    pct = (d["rps"] - base["rps"]) / base["rps"] * 100
                    parts.append(f"{t} {pct:+.1f}%")
            if parts:
                print(f"- c{cores} {sc}: " + "; ".join(parts))
    print()

    # h2 rows have no photon counterpart; print them standalone.
    for cores in core_ids:
        h2 = [(k[1], v) for k, v in agg_data.items()
              if k[0] == cores and k[1].startswith("h2-")]
        if h2:
            print(f"## h2c (h2load -m1, 无 photon 对照) c{cores}")
            for sc, d in sorted(h2):
                print(f"- {sc}: {d['rps']:,.0f} rps (mean 见原始 CSV)")
            print()

    print("## photon 官方拓扑(http-perf-client --threads 4, body 4096B)\n")
    for cores in core_ids:
        off = [(k[1], k[2], v) for k, v in agg_data.items()
               if k[0] == cores and k[1] == "client4x4096"]
        for sc, sv, d in sorted(off):
            print(f"- c{cores} {sv}: median_qps={d['rps']:,.0f}")


if __name__ == "__main__":
    main()
