#!/usr/bin/env bash
set -euo pipefail

BASE_PORT=${1:-19080}
DURATION=${2:-10}
CONNECTIONS=${3:-100}

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
OUT_DIR=${BENCH_OUT_DIR:-"$ROOT/bench/results/tcp-pingpong-$(date '+%Y%m%d-%H%M%S')"}
CORES=${TCP_PINGPONG_CORES:-$(command -v nproc >/dev/null 2>&1 && nproc || echo 1)}
DIAGNOSTICS_ENABLED=${TCP_PINGPONG_COLLECT_DIAGNOSTICS:-1}

mkdir -p "$OUT_DIR"

if ! command -v perf >/dev/null 2>&1; then
    echo "WARN: perf not installed; diagnostics disabled" >&2
    DIAGNOSTICS_ENABLED=0
fi

cd "$ROOT"

echo "output_dir=$OUT_DIR"
echo "duration=${DURATION}s connections=$CONNECTIONS cores=$CORES"

cargo build --release --example tcp_pingpong --example tcp_client --all-features

csv="$OUT_DIR/summary.csv"
printf 'name,port,cores,connections,duration,requests,qps,server_log,client_log\n' >"$csv"

server_pid=""
cleanup() {
    if [[ -n "${server_pid:-}" ]]; then
        kill "$server_pid" >/dev/null 2>&1 || true
        wait "$server_pid" >/dev/null 2>&1 || true
        server_pid=""
    fi
}
trap cleanup EXIT

start_perf_stat() {
    local pid=$1
    local duration=$2
    local perf_log=$3
    perf stat -x, -e task-clock,context-switches,cache-misses,cache-references,syscalls:sys_enter_io_uring_enter \
        -p "$pid" -- sleep "$duration" >"$perf_log" 2>&1 &
    echo $!
}

run_case() {
    local name=$1
    local cores=$2
    local port=$3
    local server_bin="$ROOT/target/release/examples/tcp_pingpong"
    local client_bin="$ROOT/target/release/examples/tcp_client"
    local server_log="$OUT_DIR/$name.server.log"
    local client_log="$OUT_DIR/$name.client.log"
    local perf_log="$OUT_DIR/$name.perf.csv"

    cleanup
    echo "== $name =="
    "$server_bin" --addr "127.0.0.1:$port" --cores "$cores" >"$server_log" 2>&1 &
    server_pid=$!

    for _ in $(seq 1 100); do
        if ss -ltn "sport = :$port" | grep -q ":$port"; then
            break
        fi
        sleep 0.1
    done
    if ! ss -ltn "sport = :$port" | grep -q ":$port"; then
        echo "ERROR: $name did not listen on port $port" >&2
        sed -n '1,80p' "$server_log" >&2 || true
        exit 1
    fi

    perf_pid=""
    if [[ "$DIAGNOSTICS_ENABLED" = "1" ]]; then
        perf_pid=$(start_perf_stat "$server_pid" "$DURATION" "$perf_log")
    fi

    local client_output
    client_output=$("$client_bin" --addr "127.0.0.1:$port" --connections "$CONNECTIONS" --duration "$DURATION" 2>&1 | tee "$client_log")

    if [[ -n "$perf_pid" ]]; then
        wait "$perf_pid" >/dev/null 2>&1 || true
    fi

    local qps
    qps=$(printf '%s\n' "$client_output" | awk '/^QPS:/ {print $2; exit}')
    local requests
    requests=$(printf '%s\n' "$client_output" | awk '/^TCP ping-pong:/ {print $3; exit}')
    if [[ -z "$qps" || -z "$requests" ]]; then
        echo "ERROR: missing client output for $name" >&2
        cat "$client_log" >&2
        exit 1
    fi

    printf '"%s","%s","%s","%s","%s","%s","%s","%s","%s"\n' \
        "$name" "$port" "$cores" "$CONNECTIONS" "$DURATION" "$requests" "$qps" "$server_log" "$client_log" | tee -a "$csv"
}

run_case "onecore" 1 "$BASE_PORT"
run_case "multicore" "$CORES" "$((BASE_PORT + 1))"

cleanup
echo "summary=$csv"
