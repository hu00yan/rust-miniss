#!/usr/bin/env bash
# bench_photon_matrix.sh — interleaved, order-rotated HTTP matrix:
#   miniss framer / miniss hyper-h1 / miniss hyper-h2c / hyper-on-tokio /
#   monoio / photon(epoll|uring|uring-sock)
# against the same wrk loader and scenarios as
# bench/scripts/bench_http.sh + bench_hyper.sh (the committed matrix):
#   warmup (-t1 -c10 -d3s) before every measured run; 3 rounds per scenario;
#   rounds rotate the server order (order-swapped interleaving);
#   MINISS_URING_ENTRIES=1024; VmRSS sampled over the measured window;
#   zero socket/HTTP errors enforced; servers killed and waited on every run.
#
# Plus the "photon official topology" reproduction:
#   http-perf-server-mc (photon's own perf server, 4096B body) loaded by
#   photon's own http-perf-client (--threads 4), both engines.
#
# Usage: bench_photon_matrix.sh <out_dir> <cores_list, e.g. "1" or "1 4 7">
# Env:
#   MINISS_DIR   repo root with target/release/examples/* and
#                bench/runtime_comparison/target/release/*
#   PHOTON_BIN   dir with http-perf-server-mc and http-perf-client
#   BENCH_DUR (10) BENCH_ROUNDS (3) BENCH_BASE_PORT (17700)
#   ENABLE_MONOIO=1 (default: on if binary exists)
#   SKIP_OFFICIAL=1 to skip the photon official-client runs
set -uo pipefail

OUT=${1:?usage: bench_photon_matrix.sh <out_dir> <"cores list">}
CORES_LIST=${2:-"1"}
MINISS_DIR=${MINISS_DIR:?set MINISS_DIR}
PHOTON_BIN=${PHOTON_BIN:?set PHOTON_BIN}
DUR=${BENCH_DUR:-10}
ROUNDS=${BENCH_ROUNDS:-3}
BASE_PORT=${BENCH_BASE_PORT:-17700}

WRK=${WRK:-$(command -v wrk || echo "$HOME/wrk-src/wrk")}
H2LOAD=${H2LOAD:-$(command -v h2load || echo /usr/bin/h2load)}

mkdir -p "$OUT"
CSV="$OUT/results.csv"
META="$OUT/meta.txt"
printf 'cores,server,scenario,round,rps,p50,p75,p90,p99,rss_kib,loader,order_note,notes\n' > "$CSV"
{
    echo "host=$(hostname) kernel=$(uname -r) nproc=$(nproc) date=$(date -Is)"
    echo "dur=$DUR rounds=$ROUNDS wrk=$($WRK --version 2>&1 | head -1)"
    echo "miniss_dir=$MINISS_DIR photon_bin=$PHOTON_BIN"
    echo "miniss_uring_entries=${MINISS_URING_ENTRIES:-unset}"
} > "$META"

port_cursor=$BASE_PORT
next_port() { port_cursor=$((port_cursor + 1)); echo "$port_cursor"; }

SERVER_PID=""
cleanup() {
    [[ -n "$SERVER_PID" ]] || return 0
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null
    SERVER_PID=""
}
trap cleanup EXIT

wait_for_port() {
    local port=$1
    for _ in $(seq 1 100); do
        if ss -ltn "sport = :$port" 2>/dev/null | grep -q ":$port"; then return 0; fi
        sleep 0.1
    done
    return 1
}

sample_rss() { # pid out_file duration
    (
        local end=$(( $(date +%s) + $3 + 1 )) max=0 rss
        while (( $(date +%s) < end )); do
            rss=$(awk '/VmRSS:/ {print $2; exit}' "/proc/$1/status" 2>/dev/null || echo 0)
            case "$rss" in ''|*[!0-9]*) rss=0;; esac
            (( rss > max )) && max=$rss
            sleep 0.2
        done
        echo "$max" > "$2"
    ) &
    echo $!
}

# start_server <tag> <cores> <port>  -> sets SERVER_PID, SERVER_LOG
start_server() {
    local tag=$1 cores=$2 port=$3
    local log="$OUT/${tag}-c${cores}.log"
    case "$tag" in
        miniss-framer)
            MINISS_URING_ENTRIES=${MINISS_URING_ENTRIES:-1024} \
                "$MINISS_DIR/target/release/examples/http_echo" \
                --addr "127.0.0.1:$port" --cores "$cores" >"$log" 2>&1 & ;;
        miniss-hyper-h1)
            MINISS_URING_ENTRIES=${MINISS_URING_ENTRIES:-1024} \
                "$MINISS_DIR/target/release/examples/http_hyper_echo" \
                --addr "127.0.0.1:$port" --cores "$cores" >"$log" 2>&1 & ;;
        miniss-hyper-h2)
            MINISS_URING_ENTRIES=${MINISS_URING_ENTRIES:-1024} \
                "$MINISS_DIR/target/release/examples/http_hyper_h2_echo" \
                --addr "127.0.0.1:$port" --cores "$cores" >"$log" 2>&1 & ;;
        hyper-tokio)
            "$MINISS_DIR/bench/runtime_comparison/target/release/hyper_tokio_echo" \
                --addr "127.0.0.1:$port" --cores "$cores" >"$log" 2>&1 & ;;
        monoio)
            "$MINISS_DIR/bench/runtime_comparison/target/release/monoio_echo" \
                --addr "127.0.0.1:$port" --cores "$cores" >"$log" 2>&1 & ;;
        photon-epoll)
            "$PHOTON_BIN/http-perf-server-mc" --port "$port" --vcpus "$cores" \
                --body_size 22 --event_engine=epoll --socket=tcp >"$log" 2>&1 & ;;
        photon-uring)
            "$PHOTON_BIN/http-perf-server-mc" --port "$port" --vcpus "$cores" \
                --body_size 22 --event_engine=iouring --socket=tcp >"$log" 2>&1 & ;;
        photon-uring-sock)
            "$PHOTON_BIN/http-perf-server-mc" --port "$port" --vcpus "$cores" \
                --body_size 22 --event_engine=iouring --socket=iouring >"$log" 2>&1 & ;;
        *)
            echo "unknown server tag $tag" >&2; return 1 ;;
    esac
    SERVER_PID=$!
    SERVER_LOG="$log"
}

H1_SERVERS="miniss-framer miniss-hyper-h1 hyper-tokio photon-epoll photon-uring photon-uring-sock"
[[ -x "$MINISS_DIR/bench/runtime_comparison/target/release/monoio_echo" && "${ENABLE_MONOIO:-1}" = "1" ]] \
    && H1_SERVERS="miniss-framer miniss-hyper-h1 hyper-tokio monoio photon-epoll photon-uring photon-uring-sock"

run_h1_server() { # tag cores scenario round threads conns close
    local tag=$1 cores=$2 scenario=$3 round=$4 threads=$5 conns=$6 close=$7
    local port; port=$(next_port)
    local raw="$OUT/${tag}-${scenario}-c${cores}-r${round}.wrk.txt"
    cleanup
    if ! start_server "$tag" "$cores" "$port"; then return 1; fi
    if ! wait_for_port "$port"; then
        echo "FAIL: $tag c$cores did not listen on $port" | tee -a "$OUT/failures.txt"
        head -5 "$SERVER_LOG" >> "$OUT/failures.txt" 2>/dev/null
        cleanup; return 1
    fi
    local headers=()
    [[ "$close" = "1" ]] && headers=(-H "Connection: close")
    "$WRK" --latency -t1 -c10 -d3s --timeout 5s "${headers[@]}" \
        "http://127.0.0.1:$port/" >/dev/null 2>&1 || true
    local rss_file="$OUT/${tag}-${scenario}-c${cores}-r${round}.rss.txt"
    local rss_pid; rss_pid=$(sample_rss "$SERVER_PID" "$rss_file" "$DUR")
    timeout 90 "$WRK" --latency -t"$threads" -c"$conns" -d"${DUR}s" --timeout 10s "${headers[@]}" \
        "http://127.0.0.1:$port/" > "$raw" 2>&1
    local wrk_rc=$?
    wait "$rss_pid" 2>/dev/null
    local rss; rss=$(cat "$rss_file" 2>/dev/null || echo 0)
    cleanup
    if [[ $wrk_rc -ne 0 ]]; then
        echo "FAIL: wrk rc=$wrk_rc $tag $scenario c$cores r$round" | tee -a "$OUT/failures.txt"
        return 1
    fi
    # parse + zero-error validation
    local line notes
    if ! line=$(perl -0777 -ne '
        if (/unable to connect/i) { print "ERR:connect"; exit }
        my ($sc, $sr, $sw, $st) = (0,0,0,0);
        if (/Socket errors:\s*connect\s+(\d+),\s*read\s+(\d+),\s*write\s+(\d+),\s*timeout\s+(\d+)/) {
            ($sc,$sr,$sw,$st)=($1,$2,$3,$4);
        }
        my $sock = $sc+$sr+$sw+$st;
        my ($http) = /Non-2xx or 3xx responses:\s*(\d+)/; $http //= 0;
        my ($rps) = /Requests\/sec:\s+(\S+)/;
        my ($p50) = /^\s*50%\s+(\S+)/m;
        my ($p99) = /^\s*99%\s+(\S+)/m;
        my ($p75) = /^\s*75%\s+(\S+)/m;
        my ($p90) = /^\s*90%\s+(\S+)/m;
        if ($sock != 0 || $http != 0 || !defined $rps || !defined $p50 || !defined $p99) {
            print "ERR:sock=$sock http=$http parse_missing"; exit
        }
        print "$rps|$p50|$p75|$p90|$p99";
    ' "$raw"); then
        line=""
    fi
    if [[ "$line" == ERR:* || -z "$line" ]]; then
        echo "FAIL: $tag $scenario c$cores r$round: $line" | tee -a "$OUT/failures.txt"
        cp "$raw" "$OUT/fail-${tag}-${scenario}-c${cores}-r${round}.txt" 2>/dev/null
        return 1
    fi
    IFS='|' read -r rps p50 p75 p90 p99 <<< "$line"
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,wrk,%s,ok\n' \
        "$cores" "$tag" "$scenario" "$round" "$rps" "$p50" "$p75" "$p90" "$p99" "$rss" "rot$round" >> "$CSV"
    echo "$tag c$cores $scenario r$round: rps=$rps p50=$p50 p99=$p99"
}

run_h2_server() { # tag cores scenario round threads conns
    local tag=$1 cores=$2 scenario=$3 round=$4 threads=$5 conns=$6
    local port; port=$(next_port)
    local raw="$OUT/${tag}-${scenario}-c${cores}-r${round}.h2load.txt"
    cleanup
    if ! start_server "$tag" "$cores" "$port"; then return 1; fi
    if ! wait_for_port "$port"; then
        echo "FAIL: $tag c$cores did not listen on $port" | tee -a "$OUT/failures.txt"
        cleanup; return 1
    fi
    "$H2LOAD" -c10 -t1 -m1 --duration=3 "http://127.0.0.1:$port/" >/dev/null 2>&1 || true
    local rss_file="$OUT/${tag}-${scenario}-c${cores}-r${round}.rss.txt"
    local rss_pid; rss_pid=$(sample_rss "$SERVER_PID" "$rss_file" "$DUR")
    timeout 90 "$H2LOAD" -c"$conns" -t"$threads" -m1 --duration="$DUR" \
        "http://127.0.0.1:$port/" > "$raw" 2>&1
    local rc=$?
    wait "$rss_pid" 2>/dev/null
    local rss; rss=$(cat "$rss_file" 2>/dev/null || echo 0)
    cleanup
    if [[ $rc -ne 0 ]]; then
        echo "FAIL: h2load rc=$rc $tag $scenario c$cores r$round" | tee -a "$OUT/failures.txt"
        return 1
    fi
    local finished rps mean
    finished=$(sed -n 's/^finished in \([0-9.]*\)s, \([0-9.]*\) req\/s.*/\1/p' "$raw" | head -1)
    rps=$(sed -n 's/^finished in [0-9.]*s, \([0-9.]*\) req\/s.*/\1/p' "$raw" | head -1)
    mean=$(grep -oE '[0-9.]+(ns|us|ms|s) mean' "$raw" | head -1 | awk '{print $1}')
    if [[ -z "$finished" || -z "$rps" || "$finished" == "0" ]]; then
        echo "FAIL: h2load unparseable $tag $scenario c$cores r$round" | tee -a "$OUT/failures.txt"
        return 1
    fi
    printf '%s,%s,%s,%s,%s,h2load_mean=%s,,,,%s,h2load,rot%s,%s\n' \
        "$cores" "$tag" "$scenario" "$round" "$rps" "${mean:-unknown}" "$rss" "$round" "$mean" >> "$CSV"
    echo "$tag c$cores $scenario r$round: rps=$rps mean=$mean"
}

run_official_client() { # cores engine
    local cores=$1 engine=$2
    local port; port=$(next_port)
    local log="$OUT/photon-official-${engine}-c${cores}.server.log"
    cleanup
    "$PHOTON_BIN/http-perf-server-mc" --port "$port" --vcpus "$cores" \
        --body_size 4096 --event_engine="$engine" --socket=tcp >"$log" 2>&1 &
    SERVER_PID=$!
    if ! wait_for_port "$port"; then
        echo "FAIL: official server ($engine c$cores) not listening" | tee -a "$OUT/failures.txt"
        cleanup; return 1
    fi
    # photon's own client: endless loop -> wrap in timeout; 13s window
    timeout 13 "$PHOTON_BIN/http-perf-client" --ip 127.0.0.1 --port "$port" \
        --threads 4 --body_size 4096 > "$OUT/photon-official-${engine}-c${cores}.client.txt" 2>&1
    cleanup
    # median of per-second server qps lines (skip first 3s as warmup)
    local med_qps
    med_qps=$(grep -oE 'qps: [0-9]+' "$log" | awk '{print $2}' | tail -n +4 | sort -n | awk '{a[NR]=$1} END {if (NR%2) print a[(NR+1)/2]; else print (a[NR/2]+a[NR/2+1])/2}')
    local lat mbps
    lat=$(grep -oE 'latency = [0-9]+us' "$OUT/photon-official-${engine}-c${cores}.client.txt" | awk '{print $3}')
    mbps=$(grep -oE 'throughput = [0-9.]+MB/s' "$OUT/photon-official-${engine}-c${cores}.client.txt" | awk '{print $3}')
    printf '%s,photon-official-%s,client4x4096,1,%s,latency_us=%s,,,,,photon-client,na,%s\n' \
        "$cores" "$engine" "${med_qps:-0}" "${lat:-unknown}" "${mbps:-unknown}" >> "$CSV"
    echo "photon-official $engine c$cores: median_qps=$med_qps client_latency=$lat us mbps=$mbps"
}

rotate() { # rotate list so it starts at offset (offset is 1-based round)
    local off=$1; shift
    local -a arr=("$@")
    local n=${#arr[@]}
    local out="" idx i
    for ((i=0; i<n; i++)); do
        idx=$(( (off - 1 + i) % n ))
        out+="${arr[$idx]} "
    done
    echo "$out"
}

for cores in $CORES_LIST; do
    echo "=== cores=$cores ==="
    # H1 scenarios, order-rotated per round
    for scenario in ka8 ka32 close32; do
        threads=1; conns=8; closemode=0
        case "$scenario" in
            ka32)   threads=2; conns=32 ;;
            close32) threads=2; conns=32; closemode=1 ;;
        esac
        for round in $(seq 1 "$ROUNDS"); do
            order=$(rotate "$round" $H1_SERVERS)
            for tag in $order; do
                run_h1_server "$tag" "$cores" "$scenario" "$round" \
                    "$threads" "$conns" "$closemode"
            done
        done
    done
    # H2 (h2c prior knowledge) — miniss hyper h2 only (wrk cannot h2c; h2load has no close mode)
    for round in $(seq 1 "$ROUNDS"); do
        run_h2_server miniss-hyper-h2 "$cores" h2-ka8  "$round" 1 8
        run_h2_server miniss-hyper-h2 "$cores" h2-ka32 "$round" 2 32
    done
    # photon official topology (their client, their 4096B default)
    if [[ "${SKIP_OFFICIAL:-0}" != "1" ]]; then
        run_official_client "$cores" epoll
        run_official_client "$cores" iouring
    fi
done

cleanup
echo
echo "leftover processes check:"
pgrep -af "http_echo|http_hyper|hyper_tokio|monoio_echo|http-perf-server" || echo "  none"
echo "failures: $(wc -l < "$OUT/failures.txt" 2>/dev/null || echo 0)"
echo "RESULTS=$CSV"
