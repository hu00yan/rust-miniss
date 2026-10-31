#!/usr/bin/env bash
set -euo pipefail

# Interleaved io_uring flag-matrix benchmark.
#
# Purpose: attribute a performance delta to ONE runtime flag at a time using
# paired, order-swapped rounds on one idle machine, together with per-round
# mechanism counters (io_uring_enter count, task-clock, context switches, max
# RSS). A throughput delta without mechanism counters is not evidence of a
# mechanism; the counters say WHY a number moved.
#
# Output: bench/results/uring-flags-<ts>/summary-<flagset>.csv (same column
# shape as bench_http.sh, so the standard verdict tool compares two flagsets)
# plus mechanism-<flagset>.csv and the raw wrk/server logs.
#
# Usage:
#   ./bench/scripts/bench_uring_flags.sh                          # defaults
#   BENCH_FLAG_ROUNDS=5 BENCH_FLAG_DURATION=10 ./bench/scripts/...
#   BENCH_FLAGSETS="default NO_COOP=MINISS_URING_NO_COOP_TASKRUN=1" ...
#
# Pairwise verdict with the existing tool:
#   out=bench/results/uring-flags-<ts>
#   BENCH_BASELINE=$out/summary-default.csv \
#     just bench-verdict $out $out/summary-NO_COOP.csv
#
# Linux only (io_uring + taskset + perf syscall tracepoints). Every server is
# stopped through the same bounded path as bench_http.sh; leaks are counted,
# never left behind.

BASE_PORT=${BASE_PORT:-9320}
DURATION=${BENCH_FLAG_DURATION:-10}
ROUNDS=${BENCH_FLAG_ROUNDS:-3}
HTTP_CORES=${HTTP_CORES:-$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)}
SERVER_CPUS=${BENCH_SERVER_CPUS:-}
CLIENT_CPUS=${BENCH_CLIENT_CPUS:-}
HTTP_CONNECTION_CLOSE=${HTTP_CONNECTION_CLOSE:-0}
CONNECTIONS=${BENCH_FLAG_CONNECTIONS:-200}
THREADS=${BENCH_FLAG_THREADS:-2}

# Each entry is NAME=VAR=value,VAR=value. The first entry is the baseline.
# Flagsets must be kernel-legal: src/io/uring_features.rs resolve_uring_flags()
# is the ONE table-driven resolver (CONSTRAINT_TABLE): DEFER promotes
# SINGLE_ISSUER and excludes COOP; the kernel rejects SQPOLL with COOP or
# DEFER (-EINVAL), so an explicit-on pair errors at startup while an auto
# side yields (recorded) — since 2026-10-31 the sqpoll flagset actually
# engages SQPOLL (the pre-resolver path sent COOP+SQPOLL, hit -EINVAL, and
# silently degraded to a plain ring), so pre/post numbers are not comparable.
# Legacy binary spellings (=1) keep working; three-state
# MINISS_URING_<FEATURE>=auto|on|off is the preferred spelling.
DEFAULT_FLAGSETS=(
    "default"
    "no_coop=MINISS_URING_NO_COOP_TASKRUN=1"
    "defer_pair=MINISS_URING_SINGLE_ISSUER=1,MINISS_URING_DEFER_TASKRUN=1"
    "sqpoll=MINISS_URING_SQPOLL=1"
    "flush16=MINISS_URING_FLUSH_BATCH_SIZE=16"
    "entries1024=MINISS_URING_ENTRIES=1024"
    "no_try_write=MINISS_NO_STATIC_TRY_WRITE=1"
)

if [[ -n "${BENCH_FLAGSETS:-}" ]]; then
    read -r -a FLAGSETS <<<"$BENCH_FLAGSETS"
else
    FLAGSETS=("${DEFAULT_FLAGSETS[@]}")
fi

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "ERROR: bench_uring_flags requires Linux (io_uring + perf tracepoints); got $(uname -s)" >&2
    exit 2
fi
if ! command -v wrk >/dev/null 2>&1; then
    echo "ERROR: wrk is not installed" >&2
    exit 2
fi

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
OUT_DIR=${BENCH_OUT_DIR:-"$ROOT/bench/results/uring-flags-$(date '+%Y%m%d-%H%M%S')"}
mkdir -p "$OUT_DIR"
cd "$ROOT"

echo "output_dir=$OUT_DIR"
echo "rounds=$ROUNDS duration=${DURATION}s connections=$CONNECTIONS threads=$THREADS http_cores=$HTTP_CORES"
echo "flagsets: ${FLAGSETS[*]}"

cargo build --release --example http_echo --all-features

# Per-flagset summary files keep the exact bench_http.sh column shape so
# bench_verdict.pl can compare any two flagsets without adaptation.
read -r -a SUMMARY_HEADER <<'EOF'
name,port,threads,connections,duration,requests,rps,avg_latency,p50,p75,p90,p99,transfer,socket_errors,http_errors,server_exit,process_leaks,raw_log,server_log
EOF

# mechanism-<flagset>.csv records WHY the throughput moved.
MECH_HEADER='round,io_uring_enter,server_cpu_s,task_clock_ms,context_switches,max_rss_kb'

server_pid=""
RSS_PID=""
PERF_PID=""

cleanup() {
    if [[ -n "${PERF_PID:-}" && "${PERF_PID}" != "0" ]]; then
        kill "$PERF_PID" >/dev/null 2>&1 || true
        wait "$PERF_PID" >/dev/null 2>&1 || true
        PERF_PID=""
    fi
    if [[ -n "${RSS_PID:-}" && "${RSS_PID}" != "0" ]]; then
        kill "$RSS_PID" >/dev/null 2>&1 || true
        wait "$RSS_PID" >/dev/null 2>&1 || true
        RSS_PID=""
    fi
    if [[ -n "${server_pid:-}" ]]; then
        kill "$server_pid" >/dev/null 2>&1 || true
        wait "$server_pid" >/dev/null 2>&1 || true
        server_pid=""
    fi
}
trap cleanup EXIT

stop_server() {
    [[ -n "${server_pid:-}" ]] || return 0
    local pid=$server_pid
    kill "$pid" >/dev/null 2>&1 || true
    local wait_status=0
    wait "$pid" >/dev/null 2>&1 || wait_status=$?
    if kill -0 "$pid" >/dev/null 2>&1; then
        echo "WARN: server $pid survived SIGTERM; sending SIGKILL" >&2
        kill -KILL "$pid" >/dev/null 2>&1 || true
        wait "$pid" >/dev/null 2>&1 || true
    fi
    server_pid=""
}

wait_for_port() {
    local port=$1
    for _ in $(seq 1 100); do
        if ss -ltn "sport = :$port" | grep -q ":$port"; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

start_rss_sampler() {
    local pid=$1
    local duration=$2
    local rss_log=$3
    (
        local end_ts=$(( $(date +%s) + duration + 1 ))
        local max_rss=0
        while (( $(date +%s) < end_ts )); do
            local rss=0
            if [[ -r "/proc/$pid/status" ]]; then
                rss=$(awk '/VmRSS:/ {print $2; exit}' "/proc/$pid/status" 2>/dev/null || echo 0)
            fi
            if [[ "$rss" =~ ^[0-9]+$ ]] && [[ "$rss" -gt "$max_rss" ]]; then
                max_rss=$rss
            fi
            sleep 0.2
        done
        printf '%s\n' "$max_rss" >"$rss_log"
    ) &
    RSS_PID=$!
}

# Mechanism-counter plan. perf syscall tracepoints need tracing access; on
# OrbStack that usually means sudo. Detect once; fall back to sudo -n, and to
# /proc/<pid>/stat CPU time when perf is unavailable entirely.
PERF_PREFIX=()
if command -v perf >/dev/null 2>&1; then
    if perf stat -x, -e syscalls:sys_enter_io_uring_enter -- true >/dev/null 2>&1; then
        PERF_PREFIX=(perf)
    elif sudo -n perf stat -x, -e syscalls:sys_enter_io_uring_enter -- true >/dev/null 2>&1; then
        PERF_PREFIX=(sudo -n perf)
        echo "note: syscall tracepoints need sudo; using sudo -n perf"
    else
        echo "note: perf tracepoints unavailable; io_uring_enter columns will read 0 (CPU time still measured via /proc)"
    fi
else
    echo "note: perf not installed; io_uring_enter columns will read 0 (CPU time still measured via /proc)"
fi

# Server CPU seconds, read from /proc before the server is stopped. Always
# available; ticks -> seconds with the standard 100 Hz assumption.
server_cpu_seconds() {
    local pid=$1
    [[ -r "/proc/$pid/stat" ]] || { echo "0"; return; }
    awk '{ cpu = ($14 + $15) / 100; printf "%.2f", cpu }' "/proc/$pid/stat"
}

start_perf_stat() {
    local pid=$1
    local duration=$2
    local perf_log=$3
    if [[ "${#PERF_PREFIX[@]}" -eq 0 ]]; then
        : >"$perf_log"
        PERF_PID=""
        return 0
    fi
    "${PERF_PREFIX[@]}" stat -x, -e task-clock,context-switches,syscalls:sys_enter_io_uring_enter \
        -p "$pid" -- sleep "$duration" >"$perf_log" 2>&1 &
    PERF_PID=$!
}

run_wrk() {
    if [[ -n "$CLIENT_CPUS" ]]; then
        taskset -c "$CLIENT_CPUS" wrk "$@"
    else
        wrk "$@"
    fi
}

# Parse one wrk --latency transcript into the shared summary columns.
parse_wrk_row() {
    local name=$1 port=$2 raw_log=$3 server_exit=$4 process_leaks=$5
    perl -0777 - "$name" "$port" "$THREADS" "$CONNECTIONS" "$DURATION" "$raw_log" "$server_exit" "$process_leaks" <<'PERL'
use strict; use warnings;
my ($name, $port, $threads, $connections, $duration, $raw_log, $server_exit, $process_leaks) = @ARGV;
open my $fh, '<', $raw_log or die "cannot read $raw_log: $!";
my $s = do { local $/; <$fh> };
die "wrk failed for $name: unable to connect\n" if $s =~ /unable to connect/i;
my ($requests) = $s =~ /(\d+)\s+requests in\s+\S+/;
my ($rps) = $s =~ /Requests\/sec:\s+(\S+)/;
my ($avg) = $s =~ /Latency\s+(\S+)/;
my ($p50) = $s =~ /^\s*50%\s+(\S+)/m;
my ($p75) = $s =~ /^\s*75%\s+(\S+)/m;
my ($p90) = $s =~ /^\s*90%\s+(\S+)/m;
my ($p99) = $s =~ /^\s*99%\s+(\S+)/m;
my ($transfer) = $s =~ /Transfer\/sec:\s+(\S+)/;
my ($socket_connect, $socket_read, $socket_write, $socket_timeout) = (0, 0, 0, 0);
if ($s =~ /Socket errors:\s*connect\s+(\d+),\s+read\s+(\d+),\s+write\s+(\d+),\s+timeout\s+(\d+)/) {
    ($socket_connect, $socket_read, $socket_write, $socket_timeout) = ($1, $2, $3, $4);
}
my ($http_errors) = $s =~ /Non-2xx or 3xx responses:\s*(\d+)/;
$http_errors //= 0;
for my $field (['rps', $rps], ['p50', $p50], ['p99', $p99], ['requests', $requests]) {
    die "missing $field->[0] in wrk output for $name\n" unless defined $field->[1] && length $field->[1];
}
print join(',', $name, $port, $threads, $connections, $duration, $requests, $rps, $avg, $p50, $p75, $p90, $p99, $transfer,
    $socket_connect + $socket_read + $socket_write + $socket_timeout, $http_errors, $server_exit, $process_leaks,
    $raw_log, 'server.log'), "\n";
PERL
}

# Extract one perf stat -x, field by event name.
perf_field() {
    local perf_log=$1 event=$2
    awk -F, -v ev="$event" '$3 == ev { gsub(/ /, "", $1); print $1; exit }' "$perf_log" 2>/dev/null || true
}

flagset_env() {
    local spec=$1
    local body=${spec#*=}
    if [[ "$body" == "$spec" ]]; then
        echo ""
    else
        echo "$body"
    fi
}

flagset_name() {
    local spec=$1
    echo "${spec%%=*}"
}

PORT=$BASE_PORT
for round in $(seq 1 "$ROUNDS"); do
    if (( round % 2 == 0 )); then
        # Order swap: baseline runs last on even rounds so a hot/cold machine
        # cannot systematically favor the first-set.
        mapfile -t order < <(printf '%s\n' "${FLAGSETS[@]}" | tac)
    else
        mapfile -t order < <(printf '%s\n' "${FLAGSETS[@]}")
    fi
    for spec in "${order[@]}"; do
        name=$(flagset_name "$spec")
        env_pairs=$(flagset_env "$spec")
        raw_log="$OUT_DIR/$name.r$round.wrk.txt"
        server_log="$OUT_DIR/$name.r$round.server.log"
        perf_log="$OUT_DIR/$name.r$round.perf.csv"
        rss_log="$OUT_DIR/$name.r$round.rss.txt"
        summary="$OUT_DIR/summary-$name.csv"
        mech="$OUT_DIR/mechanism-$name.csv"

        stop_server
        [[ -f "$summary" ]] || printf '%s\n' "$SUMMARY_HEADER" >"$summary"
        [[ -f "$mech" ]] || printf '%s\n' "$MECH_HEADER" >"$mech"

        echo "== round $round flagset=$name =="
        if [[ -n "$env_pairs" ]]; then
            IFS=',' read -r -a pair_list <<<"$env_pairs"
            env_cmd=(env "${pair_list[@]}")
        else
            env_cmd=(env)
        fi
        PORT=$((PORT + 1))

        if [[ -n "$SERVER_CPUS" ]]; then
            "${env_cmd[@]}" taskset -c "$SERVER_CPUS" \
                "$ROOT/target/release/examples/http_echo" --addr "127.0.0.1:$PORT" --cores "$HTTP_CORES" \
                >"$server_log" 2>&1 &
        else
            "${env_cmd[@]}" \
                "$ROOT/target/release/examples/http_echo" --addr "127.0.0.1:$PORT" --cores "$HTTP_CORES" \
                >"$server_log" 2>&1 &
        fi
        server_pid=$!

        if ! wait_for_port "$PORT"; then
            echo "ERROR: flagset $name did not listen on port $PORT" >&2
            sed -n '1,40p' "$server_log" >&2 || true
            exit 1
        fi

        run_wrk --latency -t1 -c10 -d3s --timeout 5s "http://127.0.0.1:$PORT/" >/dev/null

        start_rss_sampler "$server_pid" "$DURATION" "$rss_log"
        start_perf_stat "$server_pid" "$DURATION" "$perf_log"
        run_wrk --latency "-t$THREADS" "-c$CONNECTIONS" "-d${DURATION}s" --timeout 10s \
            "http://127.0.0.1:$PORT/" >"$raw_log"
        # Snapshot server CPU from /proc while the process is still alive.
        cpu_s=$(server_cpu_seconds "$server_pid")
        stop_server

        if [[ -n "${RSS_PID:-}" && "${RSS_PID}" != "0" ]]; then
            wait "$RSS_PID" >/dev/null 2>&1 || true
            RSS_PID=""
        fi
        if [[ -n "${PERF_PID:-}" && "${PERF_PID}" != "0" ]]; then
            wait "$PERF_PID" >/dev/null 2>&1 || true
            PERF_PID=""
        fi

        parse_wrk_row "$name" "$PORT" "$raw_log" 0 0 >>"$summary"

        enter_count=$(perf_field "$perf_log" 'syscalls:sys_enter_io_uring_enter')
        clock_ms=$(perf_field "$perf_log" 'task-clock')
        ctx_sw=$(perf_field "$perf_log" 'context-switches')
        max_rss=$(tail -1 "$rss_log" 2>/dev/null || echo 0)
        printf '%s,%s,%s,%s,%s,%s\n' "$round" "${enter_count:-0}" "${cpu_s:-0}" "${clock_ms:-0}" "${ctx_sw:-0}" "${max_rss:-0}" >>"$mech"
        echo "   rps=$(tail -1 "$summary" | cut -d, -f7) enter=${enter_count:-n/a} cpu_s=${cpu_s:-n/a} rss_kb=${max_rss:-0}"
    done
done

stop_server

echo
echo "Paired verdict example (default vs no_coop):"
echo "  BENCH_BASELINE=$OUT_DIR/summary-default.csv just bench-verdict $OUT_DIR $OUT_DIR/summary-no_coop.csv"
echo "Mechanism counters: $OUT_DIR/mechanism-*.csv (per round)"
