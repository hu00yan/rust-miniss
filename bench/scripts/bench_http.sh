#!/usr/bin/env bash
set -euo pipefail

BASE_PORT=${1:-9090}
DURATION=${2:-30}
CONNECTIONS=${3:-100}
THREADS=${4:-1}

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
OUT_DIR=${BENCH_OUT_DIR:-"$ROOT/bench/results/http-$(date '+%Y%m%d-%H%M%S')"}

detect_cores() {
    if command -v nproc >/dev/null 2>&1; then
        nproc
    elif command -v getconf >/dev/null 2>&1; then
        getconf _NPROCESSORS_ONLN
    else
        echo 1
    fi
}

HTTP_CORES=${HTTP_CORES:-$(detect_cores)}
METRICS_ENABLED=${HTTP_COLLECT_METRICS:-0}
BENCH_RUNTIMES=${BENCH_RUNTIMES:-"miniss,monoio,compio,tokio,glommio"}

mkdir -p "$OUT_DIR"

HOST_NAME=$(uname -n)
OS_NAME=$(uname -s)
KERNEL_RELEASE=$(uname -r)
if [[ -r /proc/cpuinfo ]]; then
    CPU_MODEL=$(awk -F: '/^(model name|Hardware)[[:space:]]*:/ { sub(/^[[:space:]]+/, "", $2); print $2; exit }' /proc/cpuinfo)
elif command -v sysctl >/dev/null 2>&1; then
    CPU_MODEL=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m)
else
    CPU_MODEL=$(uname -m)
fi
CPU_MODEL=${CPU_MODEL:-unknown}

# Keep the benchmark result self-describing. The comparator requires every
# field below to match, so a result from another host or load shape cannot be
# presented as a same-machine regression baseline.
perl -MJSON::PP=encode_json -e '
    my ($path, @values) = @ARGV;
    my @keys = qw(host os kernel cpu_model cores threads connections duration runtime);
    my %metadata; @metadata{@keys} = @values;
    open my $fh, ">", $path or die "cannot write $path: $!\n";
    print {$fh} encode_json(\%metadata), "\n";
' "$OUT_DIR/metadata.json" \
    "$HOST_NAME" "$OS_NAME" "$KERNEL_RELEASE" "$CPU_MODEL" "$HTTP_CORES" \
    "$THREADS" "$CONNECTIONS" "$DURATION" "$BENCH_RUNTIMES"

if ! command -v wrk >/dev/null 2>&1; then
    echo "ERROR: wrk is not installed" >&2
    exit 1
fi

if ! command -v ss >/dev/null 2>&1; then
    echo "ERROR: ss is required to wait for listening ports" >&2
    exit 1
fi

cd "$ROOT"

echo "output_dir=$OUT_DIR"
echo "duration=${DURATION}s connections=$CONNECTIONS threads=$THREADS http_cores=$HTTP_CORES"
if [[ "${HTTP_CONNECTION_CLOSE:-0}" = "1" ]]; then
    echo "connection=close"
fi
echo "runtimes=$BENCH_RUNTIMES"

cargo build --release --example http_echo --all-features
if [[ "$BENCH_RUNTIMES" == *"monoio"* || "$BENCH_RUNTIMES" == *"compio"* || "$BENCH_RUNTIMES" == *"tokio"* || "$BENCH_RUNTIMES" == *"glommio"* ]]; then
    cargo build --release --manifest-path bench/runtime_comparison/Cargo.toml --bins
fi
if [[ "$(uname -s)" == "Linux" && ( "$BENCH_RUNTIMES" == *"monoio"* || "$BENCH_RUNTIMES" == *"glommio"* ) ]]; then
    cargo build --release --manifest-path bench/runtime_comparison/Cargo.toml --features linux-bench --bin monoio_echo --bin glommio_echo
fi

csv="$OUT_DIR/summary.csv"
printf 'name,port,threads,connections,duration,requests,rps,avg_latency,p50,p75,p90,p99,transfer,raw_log,server_log\n' > "$csv"

server_pid=""
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

RSS_PID=""
PERF_PID=""

start_perf_stat() {
    local pid=$1
    local duration=$2
    local perf_log=$3

    if ! command -v perf >/dev/null 2>&1; then
        : >"$perf_log"
        PERF_PID=""
        return 0
    fi

    # Avoid PMU-only events: Orb VMs expose software counters and syscall
    # tracepoints even when cache-miss sampling is unavailable.
    local perf_runner=${BENCH_PERF_RUNNER:-perf}
    read -r -a perf_runner_args <<<"$perf_runner"
    "${perf_runner_args[@]}" stat -x, -e task-clock,context-switches,syscalls:sys_enter_io_uring_enter,syscalls:sys_enter_read,syscalls:sys_enter_write,syscalls:sys_enter_writev \
        -p "$pid" -- sleep "$duration" >"$perf_log" 2>&1 &
    PERF_PID=$!
}

parse_wrk() {
    local name=$1
    local port=$2
    local raw_log=$3
    local server_log=$4

    perl -0777 - "$name" "$port" "$THREADS" "$CONNECTIONS" "$DURATION" "$raw_log" "$server_log" <<'PERL'
use strict;
use warnings;

my ($name, $port, $threads, $connections, $duration, $raw_log, $server_log) = @ARGV;
open my $fh, '<', $raw_log or die "cannot read $raw_log: $!";
local $/;
my $s = <$fh>;

die "wrk failed for $name: unable to connect\n" if $s =~ /unable to connect/i;

    if ($s =~ /Socket errors:\s*connect\s+(\d+),\s*read\s+(\d+),\s*write\s+(\d+),\s*timeout\s+(\d+)/) {
        my $total = $1 + $2 + $3 + $4;
        if ($total != 0) {
        die "wrk socket errors for $name: $&\n";
        }
}

if ($s =~ /Non-2xx or 3xx responses:\s*(\d+)/) {
    die "non-success HTTP responses for $name: $1\n" if $1 != 0;
}

my ($avg) = $s =~ /Latency\s+(\S+)/;
my ($p50) = $s =~ /^\s*50%\s+(\S+)/m;
my ($p75) = $s =~ /^\s*75%\s+(\S+)/m;
my ($p90) = $s =~ /^\s*90%\s+(\S+)/m;
my ($p99) = $s =~ /^\s*99%\s+(\S+)/m;
my ($rps) = $s =~ /Requests\/sec:\s+(\S+)/;
my ($transfer) = $s =~ /Transfer\/sec:\s+(\S+)/;
my ($requests) = $s =~ /(\d+)\s+requests in\s+\S+/;

for my $field (
    ['avg_latency', $avg],
    ['p50', $p50],
    ['p75', $p75],
    ['p90', $p90],
    ['p99', $p99],
    ['requests_per_sec', $rps],
    ['transfer_per_sec', $transfer],
    ['requests', $requests],
) {
    die "missing $field->[0] in $raw_log\n" unless defined $field->[1];
}

my @row = ($name, $port, $threads, $connections, $duration, $requests, $rps, $avg, $p50, $p75, $p90, $p99, $transfer, $raw_log, $server_log);
for (@row) {
    s/"/""/g;
    $_ = qq("$_");
}
print join(',', @row), "\n";
PERL
}

run_benchmark() {
    local name=$1
    local binary=$2
    local port=$3
    shift 3
    local extra_args=("$@")
    local addr="http://127.0.0.1:$port/"
    local raw_log="$OUT_DIR/$name.wrk.txt"
    local server_log="$OUT_DIR/$name.server.log"
    local perf_log="$OUT_DIR/$name.perf.csv"
    local rss_log="$OUT_DIR/$name.rss.txt"

    cleanup
    rm -f "$raw_log" "$server_log"

    echo "== $name =="
    "$binary" --addr "127.0.0.1:$port" "${extra_args[@]}" >"$server_log" 2>&1 &
    server_pid=$!

    if ! wait_for_port "$port"; then
        echo "ERROR: $name did not listen on port $port" >&2
        sed -n '1,80p' "$server_log" >&2 || true
        exit 1
    fi

    local wrk_headers=()
    if [[ "${HTTP_CONNECTION_CLOSE:-0}" = "1" ]]; then
        wrk_headers=(-H "Connection: close")
    fi

    local perf_pid=""
    local rss_pid=""
    wrk --latency -t1 -c10 -d3s --timeout 5s "${wrk_headers[@]}" "$addr" >/dev/null

    # Start diagnostics only after warmup so task-clock/syscall/RSS samples
    # describe the measured window rather than the server startup and warmup.
    if [[ "$METRICS_ENABLED" = "1" ]]; then
        start_perf_stat "$server_pid" "$DURATION" "$perf_log"
        perf_pid=$PERF_PID
        start_rss_sampler "$server_pid" "$DURATION" "$rss_log"
        rss_pid=$RSS_PID
    fi

    wrk --latency -t"$THREADS" -c"$CONNECTIONS" -d"${DURATION}s" --timeout 10s "${wrk_headers[@]}" "$addr" >"$raw_log" 2>&1

    if [[ -n "$perf_pid" && "$perf_pid" != "0" ]]; then
        wait "$perf_pid" >/dev/null 2>&1 || true
        # perf may flush its CSV in stages after the child exits. Wait for the
        # final requested event row before parsing so a partial file cannot
        # become fake zeroes.
        for _ in $(seq 1 50); do
            grep -q 'syscalls:sys_enter_writev' "$perf_log" 2>/dev/null && break
            sleep 0.02
        done
        # Orb's shared mount can expose the final append slightly after the
        # producer has exited; keep diagnostics off the request hot path and
        # allow that last write to become visible before Perl opens the file.
        sleep 1
    fi
    if [[ -n "$rss_pid" ]]; then
        wait "$rss_pid" >/dev/null 2>&1 || true
    fi

    parse_wrk "$name" "$port" "$raw_log" "$server_log" | tee -a "$csv"
}

runtime_enabled() {
    local needle=$1
    [[ ",$BENCH_RUNTIMES," == *",$needle,"* ]]
}

port_offset=0
if runtime_enabled "miniss"; then
    run_benchmark "miniss" "$ROOT/target/release/examples/http_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "monoio" && [[ -x "$ROOT/bench/runtime_comparison/target/release/monoio_echo" ]]; then
    run_benchmark "monoio" "$ROOT/bench/runtime_comparison/target/release/monoio_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "compio" && [[ -x "$ROOT/bench/runtime_comparison/target/release/compio_echo" ]]; then
    run_benchmark "compio" "$ROOT/bench/runtime_comparison/target/release/compio_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "tokio" && [[ -x "$ROOT/bench/runtime_comparison/target/release/tokio_echo" ]]; then
    run_benchmark "tokio" "$ROOT/bench/runtime_comparison/target/release/tokio_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "glommio" && [[ -x "$ROOT/bench/runtime_comparison/target/release/glommio_echo" ]]; then
    run_benchmark "glommio" "$ROOT/bench/runtime_comparison/target/release/glommio_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
fi

cleanup
echo "summary=$csv"
