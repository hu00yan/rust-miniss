#!/usr/bin/env bash
# hyper-on-miniss HTTP echo benchmark for the gcloud cloud-shell comparison.
#
# Mirrors bench_http.sh / photon_bench.sh methodology exactly for HTTP/1.1:
# 3s warmup (-t1 -c10 -d3s), then 3 rounds x 10s measured runs over
#   ka8     (-t1 -c8,  keep-alive)
#   ka32    (-t2 -c32, keep-alive)
#   close32 (-t2 -c32, Connection: close header)
# For HTTP/2 (h2c prior knowledge) the loader is h2load, which has no
# per-request connection-close mode and no latency percentiles, so the h2
# matrix is ka8/ka32 only and latency is reported as h2load's mean.
#
# Usage: bench_hyper.sh h1|h2 [base_port]
# Env: BENCH_DUR (10) BENCH_ROUNDS (3) BENCH_OUT_DIR BENCH_CORES (1)
set -uo pipefail

PROTO=${1:?usage: bench_hyper.sh h1|h2 [base_port]}
BASE_PORT=${2:-17100}
DUR=${BENCH_DUR:-10}
ROUNDS=${BENCH_ROUNDS:-3}
CORES=${BENCH_CORES:-1}
OUT=${BENCH_OUT_DIR:-"$HOME/hyper-bench-$(date '+%Y%m%d-%H%M%S')"}

case "$PROTO" in
    h1)
        BIN="$HOME/wt-http-audit/target/release/examples/http_hyper_echo"
        LOADER=wrk
        ;;
    h2)
        BIN="$HOME/wt-http-audit/target/release/examples/http_hyper_h2_echo"
        LOADER=h2load
        ;;
    *)
        echo "unknown protocol $PROTO (h1|h2)" >&2
        exit 2
        ;;
esac

WRK=${WRK:-$(command -v wrk || echo "$HOME/bin/wrk")}
H2LOAD=${H2LOAD:-$(command -v h2load || echo /usr/bin/h2load)}
mkdir -p "$OUT"
RESULTS="$OUT/results-$PROTO.csv"
printf 'scenario,round,rps,latency_note,rss_kib,loader,server_log\n' > "$RESULTS"

idx=0
run_h1_scenario() {
    local name=$1 threads=$2 conns=$3 close=${4:-0}
    idx=$((idx + 1))
    for round in $(seq 1 "$ROUNDS"); do
        local port=$((BASE_PORT + idx * 10 + round))
        local log="$OUT/$name-r$round.server.log"
        MINISS_URING_ENTRIES=${MINISS_URING_ENTRIES:-1024} "$BIN" --addr "127.0.0.1:$port" --cores "$CORES" >"$log" 2>&1 &
        local spid=$!
        local ok=0
        for _ in $(seq 1 100); do
            if ss -ltn "sport = :$port" 2>/dev/null | grep -q ":$port"; then ok=1; break; fi
            sleep 0.1
        done
        if [[ "$ok" != "1" ]]; then
            echo "FAIL: server did not listen on $port ($name r$round)"; cat "$log"
            kill "$spid" 2>/dev/null; wait "$spid" 2>/dev/null
            continue
        fi
        local headers=()
        [[ "$close" = "1" ]] && headers=(-H "Connection: close")
        "$WRK" --latency -t1 -c10 -d3s --timeout 5s "${headers[@]}" "http://127.0.0.1:$port/" >/dev/null 2>&1 || true
        (
            end=$(( $(date +%s) + DUR + 1 ))
            max=0
            while (( $(date +%s) < end )); do
                rss=$(awk '/VmRSS:/ {print $2; exit}' "/proc/$spid/status" 2>/dev/null || echo 0)
                case "$rss" in ''|*[!0-9]*) rss=0;; esac
                (( rss > max )) && max=$rss
                sleep 0.2
            done
            printf '%s\n' "$max" > "$OUT/$name-r$round.rss.txt"
        ) &
        local rpid=$!
        "$WRK" --latency -t"$threads" -c"$conns" -d"${DUR}s" --timeout 10s "${headers[@]}" \
            "http://127.0.0.1:$port/" > "$OUT/$name-r$round.wrk.txt" 2>&1
        wait "$rpid" 2>/dev/null
        kill "$spid" 2>/dev/null; wait "$spid" 2>/dev/null
        local rps p50 p75 p90 p99 rss
        rps=$(awk '/Requests\/sec/ {print $2}' "$OUT/$name-r$round.wrk.txt")
        p50=$(awk '$1=="50%" {print $2}' "$OUT/$name-r$round.wrk.txt" | head -1)
        p75=$(awk '$1=="75%" {print $2}' "$OUT/$name-r$round.wrk.txt" | head -1)
        p90=$(awk '$1=="90%" {print $2}' "$OUT/$name-r$round.wrk.txt" | head -1)
        p99=$(awk '$1=="99%" {print $2}' "$OUT/$name-r$round.wrk.txt" | head -1)
        rss=$(cat "$OUT/$name-r$round.rss.txt" 2>/dev/null || echo 0)
        printf '%s,%s,%s,p50=%s p75=%s p90=%s p99=%s,%s,%s,%s\n' \
            "$name" "$round" "$rps" "$p50" "$p75" "$p90" "$p99" "$rss" "wrk" "$log" >> "$RESULTS"
        echo "$name r$round: rps=$rps p50=$p50 p99=$p99 rss_kib=$rss"
        sleep 1
    done
}

run_h2_scenario() {
    local name=$1 threads=$2 conns=$3
    idx=$((idx + 1))
    for round in $(seq 1 "$ROUNDS"); do
        local port=$((BASE_PORT + idx * 10 + round))
        local log="$OUT/$name-r$round.server.log"
        MINISS_URING_ENTRIES=${MINISS_URING_ENTRIES:-1024} "$BIN" --addr "127.0.0.1:$port" --cores "$CORES" >"$log" 2>&1 &
        local spid=$!
        local ok=0
        for _ in $(seq 1 100); do
            if ss -ltn "sport = :$port" 2>/dev/null | grep -q ":$port"; then ok=1; break; fi
            sleep 0.1
        done
        if [[ "$ok" != "1" ]]; then
            echo "FAIL: server did not listen on $port ($name r$round)"; cat "$log"
            kill "$spid" 2>/dev/null; wait "$spid" 2>/dev/null
            continue
        fi
        "$H2LOAD" -c10 -t1 -m1 --duration=3 "http://127.0.0.1:$port/" >/dev/null 2>&1 || true
        (
            end=$(( $(date +%s) + DUR + 1 ))
            max=0
            while (( $(date +%s) < end )); do
                rss=$(awk '/VmRSS:/ {print $2; exit}' "/proc/$spid/status" 2>/dev/null || echo 0)
                case "$rss" in ''|*[!0-9]*) rss=0;; esac
                (( rss > max )) && max=$rss
                sleep 0.2
            done
            printf '%s\n' "$max" > "$OUT/$name-r$round.rss.txt"
        ) &
        local rpid=$!
        "$H2LOAD" -c"$conns" -t"$threads" -m1 --duration="$DUR" \
            "http://127.0.0.1:$port/" > "$OUT/$name-r$round.h2load.txt" 2>&1
        wait "$rpid" 2>/dev/null
        kill "$spid" 2>/dev/null; wait "$spid" 2>/dev/null
        local finished total rps mean rss
        # h2load 1.59 prints: "finished in 10.00s, 3344.50 req/s, 157.03KB/s"
        finished=$(sed -n 's/^finished in \([0-9.]*\)s, \([0-9.]*\) req\/s.*/\1/p' "$OUT/$name-r$round.h2load.txt" | head -1)
        rps=$(sed -n 's/^finished in [0-9.]*s, \([0-9.]*\) req\/s.*/\1/p' "$OUT/$name-r$round.h2load.txt" | head -1)
        mean=$(grep -oE '[0-9.]+(ns|us|ms|s) mean' "$OUT/$name-r$round.h2load.txt" | head -1 | awk '{print $1}')
        rss=$(cat "$OUT/$name-r$round.rss.txt" 2>/dev/null || echo 0)
        if [[ -z "$finished" || -z "$rps" || "$finished" == "0" ]]; then
            echo "FAIL: h2load output unparseable ($name r$round)"; head -30 "$OUT/$name-r$round.h2load.txt"
            continue
        fi
        printf '%s,%s,%s,h2load_mean=%s,%s,%s,%s\n' \
            "$name" "$round" "$rps" "${mean:-unknown}" "$rss" "h2load" "$log" >> "$RESULTS"
        echo "$name r$round: rps=$rps mean=$mean rss_kib=$rss"
        sleep 1
    done
}

if [[ "$PROTO" == h1 ]]; then
    run_h1_scenario ka8     1 8  0
    run_h1_scenario ka32    2 32 0
    run_h1_scenario close32 2 32 1
else
    run_h2_scenario ka8     1 8
    run_h2_scenario ka32    2 32
fi

echo "RESULTS=$RESULTS"
column -t -s, "$RESULTS" 2>/dev/null || cat "$RESULTS"
