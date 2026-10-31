#!/usr/bin/env perl
# Park-efficiency gate (runtime-safety ledger #46).
#
# Fails when an executor worker spins instead of blocking: the ledger measured
# busy_rounds/polled ~17:1 with parks collapsed to ~8k/s on a 7-worker x 32-
# connection run — ~1 core burned per worker re-scanning empty queues between
# events.  A healthy event-driven worker keeps busy_rounds/polled low (every
# productive round polls a task or drains a CQE) and parks whenever the CQ
# peek and the ready queue are empty.
#
# Input: one or more http_echo server logs containing `miniss.metrics.core`
# lines (enable with MINISS_METRICS_INTERVAL_MS=<ms> on the server).  Counters
# are cumulative, so the last line per core id is the run total.
#
# Gate:
#   1. every core with polled >= PARK_MIN_POLLED must satisfy
#      busy_rounds/polled <= PARK_MAX_BUSY_RATIO (default 5),
#   2. every such core must have parks > 0 (a busy worker that never blocked
#      is the spin signature even at ratio 1),
#   3. at least one core metrics line must exist — a gate fed a log without
#      metrics must fail loudly, never pass silently.
#
# Env overrides: PARK_MAX_BUSY_RATIO, PARK_MIN_POLLED, PARK_MIN_CORES.
use strict;
use warnings;

my $max_ratio = $ENV{PARK_MAX_BUSY_RATIO} // 5;
my $min_polled = $ENV{PARK_MIN_POLLED} // 1000;
my $min_cores = $ENV{PARK_MIN_CORES} // 1;

my @logs = @ARGV or die "usage: park_efficiency_check.pl SERVER_LOG [SERVER_LOG...]\n";

my %core;      # id => {field => value}
my $saw_metrics = 0;
for my $log (@logs) {
    open my $fh, '<', $log or die "cannot read $log: $!\n";
    while (my $line = <$fh>) {
        next unless $line =~ /^miniss\.metrics\.core id=(\d+) /;
        my $id = $1;
        my %f;
        for my $pair (split ' ', $line) {
            next unless $pair =~ /^([a-z_]+)=(\d+)$/;
            $f{$1} = $2;
        }
        next unless defined $f{busy_rounds} && defined $f{polled} && defined $f{parks};
        # Cumulative counters: the last snapshot per core wins.
        $core{$id} = \%f;
        $saw_metrics = 1;
    }
    close $fh;
}

die "PARK-EFFICIENCY FAIL: no miniss.metrics.core lines found in [@logs] — "
  . "run the server with MINISS_METRICS_INTERVAL_MS set; the gate cannot verify\n"
  unless $saw_metrics;

my $busy_cores = 0;
my @failures;
my @rows;
for my $id (sort { $a <=> $b } keys %core) {
    my $f = $core{$id};
    my ($polled, $busy, $parks, $park_us) = @$f{qw(polled busy_rounds parks park_total_us)};
    next unless $polled >= $min_polled;
    $busy_cores++;
    my $ratio = $polled > 0 ? $busy / $polled : 0;
    my $park_ms_s = sprintf '%.1f', $park_us / 1000;
    push @rows, sprintf 'core %d: polled=%d busy_rounds=%d ratio=%.2f parks=%d park_total_ms=%d',
        $id, $polled, $busy, $ratio, $parks, $park_ms_s;
    push @failures, sprintf 'core %d spins: busy_rounds/polled = %d/%d = %.2f > %.2f (ledger #46 signature)',
        $id, $busy, $polled, $ratio, $max_ratio if $ratio > $max_ratio;
    push @failures, sprintf 'core %d never parks: parks=0 over %d polled tasks (event-driven block absent)',
        $id, $polled if $parks == 0;
}

die "PARK-EFFICIENCY FAIL: no core reached polled >= $min_polled in [@logs] — "
  . "gate needs a loaded run\n"
  unless $busy_cores >= $min_cores;

print "PARK-EFFICIENCY: $busy_cores busy core(s)\n";
print "  $_\n" for @rows;
if (@failures) {
    print STDERR "PARK-EFFICIENCY FAIL (ledger #46 spin signature):\n";
    print STDERR "  $_\n" for @failures;
    exit 1;
}
printf "PARK-EFFICIENCY PASS: all busy cores at ratio <= %.2f with parks > 0\n", $max_ratio;
exit 0;
