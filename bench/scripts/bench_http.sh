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
DIAGNOSTICS_ENABLED=${HTTP_COLLECT_DIAGNOSTICS:-0}
BENCH_RUNTIMES=${BENCH_RUNTIMES:-"miniss-onecore,miniss-multicore,monoio,compio,tokio,glommio"}

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

cargo build --release --example http_echo_onecore --all-features
cargo build --release --example http_echo --all-features
if [[ "$BENCH_RUNTIMES" == *"monoio"* || "$BENCH_RUNTIMES" == *"compio"* || "$BENCH_RUNTIMES" == *"tokio"* || "$BENCH_RUNTIMES" == *"glommio"* ]]; then
    cargo build --release --manifest-path bench/runtime_comparison/Cargo.toml --bins
fi

csv="$OUT_DIR/summary.csv"
printf 'name,port,threads,connections,duration,requests,rps,avg_latency,p50,p75,p90,p99,transfer,raw_log,server_log\n' > "$csv"

diag_csv="$OUT_DIR/diagnostics.csv"
if [[ "$DIAGNOSTICS_ENABLED" = "1" ]]; then
    printf 'name,port,threads,connections,duration,requests,rps,p99,error_rate_pct,cpu_util_pct,max_rss_kb,io_uring_enter_per_sec,context_switches_per_sec,cache_miss_rate\n' > "$diag_csv"
fi

server_pid=""
cleanup() {
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
    echo $!
}

start_perf_stat() {
    local pid=$1
    local duration=$2
    local perf_log=$3

    if ! command -v perf >/dev/null 2>&1; then
        : >"$perf_log"
        echo ""
        return 0
    fi

    perf stat -x, -e task-clock,context-switches,cache-misses,cache-references,syscalls:sys_enter_io_uring_enter \
        -p "$pid" -- sleep "$duration" >"$perf_log" 2>&1 &
    echo $!
}

parse_diag_row() {
    local name=$1
    local port=$2
    local raw_log=$3
    local perf_log=$4
    local rss_log=$5
    local duration=$6

    perl -0777 - "$name" "$port" "$THREADS" "$CONNECTIONS" "$duration" "$raw_log" "$perf_log" "$rss_log" "$HTTP_CORES" <<'PERL'
use strict;
use warnings;

my ($name, $port, $threads, $connections, $duration, $raw_log, $perf_log, $rss_log, $http_cores) = @ARGV;

open my $wrk, '<', $raw_log or die "cannot read $raw_log: $!";
local $/;
my $s = <$wrk>;

my ($rps) = $s =~ /Requests\/sec:\s+(\S+)/;
my ($p99) = $s =~ /^\s*99%\s+(\S+)/m;
my ($requests) = $s =~ /(\d+)\s+requests in\s+\S+/;
die "missing wrk fields in $raw_log\n" unless defined $rps && defined $p99 && defined $requests;

my $error_rate = 0;
if ($s =~ /Socket errors:\s*connect\s+(\d+),\s*read\s+(\d+),\s*write\s+(\d+),\s*timeout\s+(\d+)/) {
    my $total = $1 + $2 + $3 + $4;
    $error_rate = $requests > 0 ? ($total / $requests) * 100.0 : 0;
}

my %perf;
if (-s $perf_log) {
    open my $pfh, '<', $perf_log or die "cannot read $perf_log: $!";
    while (my $line = <$pfh>) {
        chomp $line;
        next if $line =~ /^\s*#/;
        next unless $line =~ /,/;
        my @fields = split /,/, $line, -1;
        next unless @fields >= 3;
        my $value = $fields[0];
        my $event = $fields[2];
        $value =~ s/[[:space:]]//g;
        next unless $value =~ /^[0-9]+(?:\.[0-9]+)?$/;
        $perf{$event} = 0 + $value;
    }
}

my $task_clock_ms = $perf{'task-clock'} // 0;
my $context_switches = $perf{'context-switches'} // 0;
my $cache_misses = $perf{'cache-misses'} // 0;
my $cache_refs = $perf{'cache-references'} // 0;
my $io_uring_enter = $perf{'syscalls:sys_enter_io_uring_enter'} // 0;

my $cpu_util_pct = 0;
if ($duration > 0 && $http_cores > 0) {
    $cpu_util_pct = ($task_clock_ms / ($duration * 1000.0)) * 100.0 / $http_cores;
}

my $io_uring_enter_per_sec = $duration > 0 ? $io_uring_enter / $duration : 0;
my $context_switches_per_sec = $duration > 0 ? $context_switches / $duration : 0;
my $cache_miss_rate = $cache_refs > 0 ? $cache_misses / $cache_refs : 0;

my $max_rss_kb = 0;
if (-s $rss_log) {
    open my $rfh, '<', $rss_log or die "cannot read $rss_log: $!";
    my $rss = <$rfh>;
    chomp $rss if defined $rss;
    $max_rss_kb = 0 + ($rss // 0);
}

my @row = (
    $name,
    $port,
    $threads,
    $connections,
    $duration,
    $requests,
    $rps,
    $p99,
    sprintf('%.4f', $error_rate),
    sprintf('%.2f', $cpu_util_pct),
    $max_rss_kb,
    sprintf('%.2f', $io_uring_enter_per_sec),
    sprintf('%.2f', $context_switches_per_sec),
    sprintf('%.6f', $cache_miss_rate),
);
for (@row) {
    s/"/""/g;
    $_ = qq("$_");
}
print join(',', @row), "\n";
PERL
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
        warn "wrk socket errors for $name: $&\n";
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
    if [[ "$DIAGNOSTICS_ENABLED" = "1" ]]; then
        perf_pid=$(start_perf_stat "$server_pid" "$DURATION" "$perf_log")
        rss_pid=$(start_rss_sampler "$server_pid" "$DURATION" "$rss_log")
    fi

    wrk --latency -t1 -c10 -d3s --timeout 5s "${wrk_headers[@]}" "$addr" >/dev/null
    wrk --latency -t"$THREADS" -c"$CONNECTIONS" -d"${DURATION}s" --timeout 10s "${wrk_headers[@]}" "$addr" >"$raw_log" 2>&1

    if [[ -n "$perf_pid" && "$perf_pid" != "0" ]]; then
        wait "$perf_pid" >/dev/null 2>&1 || true
    fi
    if [[ -n "$rss_pid" ]]; then
        wait "$rss_pid" >/dev/null 2>&1 || true
    fi

    parse_wrk "$name" "$port" "$raw_log" "$server_log" | tee -a "$csv"
    if [[ "$DIAGNOSTICS_ENABLED" = "1" ]]; then
        parse_diag_row "$name" "$port" "$raw_log" "$perf_log" "$rss_log" "$DURATION" | tee -a "$diag_csv"
    fi
}

runtime_enabled() {
    local needle=$1
    [[ ",$BENCH_RUNTIMES," == *",$needle,"* ]]
}

port_offset=0
if runtime_enabled "miniss-onecore"; then
    run_benchmark "miniss-onecore" "$ROOT/target/release/examples/http_echo_onecore" "$((BASE_PORT + port_offset))"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "miniss-multicore"; then
    run_benchmark "miniss-multicore" "$ROOT/target/release/examples/http_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "monoio"; then
    run_benchmark "monoio" "$ROOT/bench/runtime_comparison/target/release/monoio_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "compio"; then
    run_benchmark "compio" "$ROOT/bench/runtime_comparison/target/release/compio_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "tokio"; then
    run_benchmark "tokio" "$ROOT/bench/runtime_comparison/target/release/tokio_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
    port_offset=$((port_offset + 1))
fi
if runtime_enabled "glommio" && [[ -x "$ROOT/bench/runtime_comparison/target/release/glommio_echo" ]]; then
    run_benchmark "glommio" "$ROOT/bench/runtime_comparison/target/release/glommio_echo" "$((BASE_PORT + port_offset))" --cores "$HTTP_CORES"
fi

cleanup
echo "summary=$csv"
