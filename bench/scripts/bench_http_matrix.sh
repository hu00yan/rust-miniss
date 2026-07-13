#!/usr/bin/env bash
set -euo pipefail

BASE_PORT=${1:-15000}

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
MATRIX_DIR=${BENCH_MATRIX_DIR:-"$ROOT/bench/results/http-matrix-$(date '+%Y%m%d-%H%M%S')"}
LIGHT_MODE=${HTTP_MATRIX_LIGHT:-0}
BASELINE_SCENARIO=${HTTP_MATRIX_BASELINE_SCENARIO:-"ka200:0:200:8:5"}
BASELINE_RUNTIMES=${HTTP_MATRIX_BASELINE_RUNTIMES:-"miniss,monoio,compio"}

# Format: name:connection_close:connections:threads:duration_seconds
DEFAULT_SCENARIOS=(
    "ka8:0:8:8:5"
    "ka32:0:32:8:5"
    "ka200:0:200:8:5"
    "ka800:0:800:8:10"
    "close8:1:8:8:5"
    "close32:1:32:8:5"
    "close800:1:800:8:10"
)

if [[ -n "${HTTP_MATRIX_SCENARIOS:-}" ]]; then
    read -r -a SCENARIOS <<<"$HTTP_MATRIX_SCENARIOS"
else
    SCENARIOS=("${DEFAULT_SCENARIOS[@]}")
fi

mkdir -p "$MATRIX_DIR"

matrix_csv="$MATRIX_DIR/matrix.csv"
ratio_csv="$MATRIX_DIR/ratios.csv"

printf 'scenario,name,rps,p50,p75,p90,p99,summary\n' >"$matrix_csv"
printf 'scenario,miniss_rps,monoio_rps,best_runtime,best_rps,miniss_vs_monoio,miniss_vs_best,summary\n' >"$ratio_csv"

port=$BASE_PORT
run_matrix_case() {
    local scenario=$1
    local runtimes=$2
    local out_dir="$MATRIX_DIR/$scenario"

    IFS=: read -r name close connections threads duration <<<"$scenario"

    echo "== scenario=$name runtimes=$runtimes close=$close connections=$connections threads=$threads duration=${duration}s port=$port =="

    if [[ "$close" = "1" ]]; then
        HTTP_CONNECTION_CLOSE=1 BENCH_RUNTIMES="$runtimes" BENCH_OUT_DIR="$out_dir" \
            "$ROOT/bench/scripts/bench_http.sh" "$port" "$duration" "$connections" "$threads"
    else
        BENCH_RUNTIMES="$runtimes" BENCH_OUT_DIR="$out_dir" \
            "$ROOT/bench/scripts/bench_http.sh" "$port" "$duration" "$connections" "$threads"
    fi

    perl -MList::Util=max - "$name" "$out_dir/summary.csv" "$matrix_csv" "$ratio_csv" "$runtimes" <<'PERL'
use strict;
use warnings;
use List::Util qw(max);

my ($scenario, $summary, $matrix_csv, $ratio_csv, $runtimes) = @ARGV;
open my $fh, '<', $summary or die "cannot read $summary: $!";
my $header = <$fh>;
my %rps;
my @rows;
while (my $line = <$fh>) {
    chomp $line;
    $line =~ s/^"//;
    $line =~ s/"$//;
    my @f = split /","/, $line;
    my ($name, $rps, $p50, $p75, $p90, $p99) = @f[0, 6, 8, 9, 10, 11];
    $rps{$name} = 0 + $rps;
    push @rows, [$name, $rps, $p50, $p75, $p90, $p99];
}

my $has_comparison = index($runtimes, 'monoio') >= 0 || index($runtimes, 'compio') >= 0 || index($runtimes, 'tokio') >= 0 || index($runtimes, 'glommio') >= 0;
open my $m, '>>', $matrix_csv or die "cannot append $matrix_csv: $!";
for my $row (@rows) {
    print $m join(',', map { qq("$_") } ($scenario, @$row, $summary)), "\n";
}

my $miniss = $rps{'miniss'} // 0;
my $monoio = $rps{'monoio'} // 0;
my ($best_name, $best_rps) = ('', 0);
for my $name (keys %rps) {
    if ($rps{$name} > $best_rps) {
        ($best_name, $best_rps) = ($name, $rps{$name});
    }
}

if ($has_comparison) {
    my $vs_monoio = $monoio > 0 ? $miniss / $monoio : 0;
    my $vs_best = $best_rps > 0 ? $miniss / $best_rps : 0;
    open my $r, '>>', $ratio_csv or die "cannot append $ratio_csv: $!";
    print $r join(',', map { qq("$_") } (
        $scenario,
        sprintf('%.2f', $miniss),
        sprintf('%.2f', $monoio),
        $best_name,
        sprintf('%.2f', $best_rps),
        sprintf('%.4f', $vs_monoio),
        sprintf('%.4f', $vs_best),
        $summary,
    )), "\n";
}
PERL

    port=$((port + 10))
}

if [[ "$LIGHT_MODE" = "1" ]]; then
    run_matrix_case "$BASELINE_SCENARIO" "$BASELINE_RUNTIMES"
    for scenario in "${SCENARIOS[@]}"; do
        if [[ "$scenario" == "$BASELINE_SCENARIO" ]]; then
            continue
        fi
        run_matrix_case "$scenario" "miniss"
    done
else
    for scenario in "${SCENARIOS[@]}"; do
        run_matrix_case "$scenario" "miniss,monoio,compio,tokio,glommio"
    done
fi

echo "matrix=$matrix_csv"
echo "ratios=$ratio_csv"
cat "$ratio_csv"
