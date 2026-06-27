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

mkdir -p "$OUT_DIR"

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

cargo build --release --example http_echo_onecore --all-features
cargo build --release --example http_echo --all-features
cargo build --release --manifest-path bench/runtime_comparison/Cargo.toml --bins

csv="$OUT_DIR/summary.csv"
printf 'name,port,threads,connections,duration,requests,rps,avg_latency,p50,p75,p90,p99,transfer,raw_log,server_log\n' > "$csv"

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
    die "wrk socket errors for $name: $&\n" if $total != 0;
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

    wrk --latency -t1 -c10 -d3s --timeout 5s "$addr" >/dev/null
    wrk --latency -t"$THREADS" -c"$CONNECTIONS" -d"${DURATION}s" --timeout 10s "$addr" >"$raw_log" 2>&1

    parse_wrk "$name" "$port" "$raw_log" "$server_log" | tee -a "$csv"
}

	run_benchmark "miniss-onecore" "$ROOT/target/release/examples/http_echo_onecore" "$BASE_PORT"
	run_benchmark "miniss-multicore" "$ROOT/target/release/examples/http_echo" "$((BASE_PORT + 1))" --cores "$HTTP_CORES"
	run_benchmark "monoio" "$ROOT/bench/runtime_comparison/target/release/monoio_echo" "$((BASE_PORT + 2))" --cores "$HTTP_CORES"
	run_benchmark "compio" "$ROOT/bench/runtime_comparison/target/release/compio_echo" "$((BASE_PORT + 3))" --cores "$HTTP_CORES"
	run_benchmark "tokio" "$ROOT/bench/runtime_comparison/target/release/tokio_echo" "$((BASE_PORT + 4))" --cores "$HTTP_CORES"

	if [[ -x "$ROOT/bench/runtime_comparison/target/release/glommio_echo" ]]; then
	    run_benchmark "glommio" "$ROOT/bench/runtime_comparison/target/release/glommio_echo" "$((BASE_PORT + 5))" --cores "$HTTP_CORES"
	fi

cleanup
echo "summary=$csv"
