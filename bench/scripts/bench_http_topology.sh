#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

: "${BENCH_SERVER_CPUS:?set BENCH_SERVER_CPUS to a non-overlapping server CPU list}"
: "${BENCH_CLIENT_CPUS:?set BENCH_CLIENT_CPUS to a non-overlapping client CPU list}"

MATRIX_DIR=${BENCH_TOPOLOGY_DIR:-"$ROOT/bench/results/http-topology-$(date '+%Y%m%d-%H%M%S')"}
ROUNDS=${BENCH_MATRIX_ROUNDS:-1}
BASE_PORT=${BENCH_TOPOLOGY_BASE_PORT:-18000}
WORKERS=${BENCH_TOPOLOGY_WORKERS:-"1 2 4 8"}
THREADS=${BENCH_TOPOLOGY_THREADS:-"1 2 4"}

if ! [[ "$ROUNDS" =~ ^[1-9][0-9]*$ ]]; then
    echo "BENCH_MATRIX_ROUNDS must be a positive integer" >&2
    exit 2
fi

if [[ -n "${BENCH_TOPOLOGY_SCENARIOS:-}" ]]; then
    read -r -a raw_scenarios <<<"$BENCH_TOPOLOGY_SCENARIOS"
else
    raw_scenarios=(
        "ka8:0:8:5"
        "ka32:0:32:5"
        "ka200:0:200:5"
        "ka800:0:800:10"
        "close8:1:8:5"
        "close32:1:32:5"
        "close800:1:800:10"
    )
fi

mkdir -p "$MATRIX_DIR"
port=$BASE_PORT

for workers in $WORKERS; do
    if ! [[ "$workers" =~ ^[1-9][0-9]*$ ]]; then
        echo "invalid worker count: $workers" >&2
        exit 2
    fi
    for threads in $THREADS; do
        if ! [[ "$threads" =~ ^[1-9][0-9]*$ ]]; then
            echo "invalid client thread count: $threads" >&2
            exit 2
        fi

        scenarios=()
        for raw in "${raw_scenarios[@]}"; do
            IFS=: read -r name close connections duration <<<"$raw"
            scenarios+=("$name:$close:$connections:$threads:$duration")
        done

        topology_dir="$MATRIX_DIR/workers-${workers}-threads-${threads}"
        echo "== topology workers=$workers client_threads=$threads server_cpus=$BENCH_SERVER_CPUS client_cpus=$BENCH_CLIENT_CPUS =="
        HTTP_CORES="$workers" \
        BENCH_SERVER_CPUS="$BENCH_SERVER_CPUS" \
        BENCH_CLIENT_CPUS="$BENCH_CLIENT_CPUS" \
        BENCH_MATRIX_ROUNDS="$ROUNDS" \
        BENCH_MATRIX_DIR="$topology_dir" \
        HTTP_MATRIX_SCENARIOS="${scenarios[*]}" \
            "$ROOT/bench/scripts/bench_http_matrix.sh" "$port"
        port=$((port + 1000))
    done
done

echo "topology_matrix=$MATRIX_DIR"
