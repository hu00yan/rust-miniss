#!/usr/bin/env perl
use strict;
use warnings;
use JSON::PP qw(encode_json);
use Text::ParseWords qw(parse_line);
use List::Util qw(sum);

sub usage { die "usage: bench_verdict.pl OUTPUT.json SUMMARY.csv [...summary.csv]\n" }
my $output = shift @ARGV or usage();
my @paths = @ARGV or usage();
my %rows;
for my $path (@paths) {
    open my $fh, '<', $path or die "cannot read $path: $!\n";
    my $header = <$fh> // die "empty summary: $path\n";
    chomp $header;
    my @keys = parse_line(',', 0, $header);
    while (my $line = <$fh>) {
        chomp $line;
        next unless length $line;
        my @values = parse_line(',', 0, $line);
        my %row; @row{@keys} = @values;
        die "summary row missing name in $path\n" unless defined $row{name};
        push @{$rows{$row{name}}}, \%row;
    }
}

sub median {
    my @v = sort { $a <=> $b } @_;
    return 0 unless @v;
    my $n = @v;
    return $n % 2 ? $v[int($n/2)] : ($v[$n/2-1] + $v[$n/2]) / 2;
}
sub mad {
    my ($m, @v) = @_;
    return median(map { abs($_ - $m) } @v);
}
sub number {
    return 0 unless defined($_[0]) && $_[0] ne '';
    return 0 + $1 if $_[0] =~ /^\s*([+-]?(?:\d+(?:\.\d*)?|\.\d+))/;
    die "non-numeric benchmark value: $_[0]\n";
}
sub latency_us {
    return 0 unless defined($_[0]) && $_[0] ne '';
    my $value = $_[0];
    $value =~ /^\s*([+-]?(?:\d+(?:\.\d*)?|\.\d+))\s*(us|µs|ms|s)?/i
        or die "non-numeric latency value: $value\n";
    my ($n, $unit) = ($1 + 0, lc($2 // 'us'));
    return $n if $unit eq 'us' || $unit eq 'µs';
    return $n * 1_000 if $unit eq 'ms';
    return $n * 1_000_000 if $unit eq 's';
    die "unsupported latency unit: $unit\n";
}

my $max_cv = 0 + ($ENV{BENCH_MAX_CV} // 0.10);
my $max_p99_mad_ratio = 0 + ($ENV{BENCH_MAX_P99_MAD_RATIO} // 0.20);
my $min_rounds = 0 + ($ENV{BENCH_MIN_ROUNDS} // 3);
my $baseline = $ENV{BENCH_BASELINE};
my (%baseline_rps, %baseline_p99);
if (defined $baseline && length $baseline) {
    open my $bfh, '<', $baseline or die "cannot read baseline $baseline: $!\n";
    my $h = <$bfh> // die "empty baseline\n"; chomp $h;
    my @k = parse_line(',', 0, $h);
    while (my $line = <$bfh>) {
        chomp $line; next unless length $line;
        my @v = parse_line(',', 0, $line); my %r; @r{@k} = @v;
        if (defined $r{name}) {
            $baseline_rps{$r{name}} = number($r{rps});
            $baseline_p99{$r{name}} = latency_us($r{p99_us} // $r{p99});
        }
    }
}

my @verdict_rows; my $passed = 1;
for my $name (sort keys %rows) {
    my @r = @{$rows{$name}};
    my @rps = map { number($_->{rps}) } @r;
    my @p99 = map { latency_us($_->{p99_us} // $_->{p99}) } @r;
    my $rps_median = median(@rps);
    my $p99_median = median(@p99);
    my $rps_mean = @rps ? sum(@rps) / @rps : 0;
    my $variance = @rps ? sum(map { ($_ - $rps_mean) ** 2 } @rps) / @rps : 0;
    my $cv = $rps_mean ? sqrt($variance) / $rps_mean : 0;
    my $p99_mad = mad($p99_median, @p99);
    my $socket_errors = sum(map { number($_->{socket_errors}) } @r);
    my $http_errors = sum(map { number($_->{http_errors}) } @r);
    my $leaks = sum(map { number($_->{process_leaks}) } @r);
    my $baseline_ratio = 1;
    $baseline_ratio = $rps_median / $baseline_rps{$name} if $baseline_rps{$name};
    my $baseline_p99_ratio = 1;
    $baseline_p99_ratio = $p99_median / $baseline_p99{$name} if $baseline_p99{$name};
    my $runtime_pass = @r >= $min_rounds && $socket_errors == 0 && $http_errors == 0 && $leaks == 0
        && $cv <= $max_cv
        && ($p99_median == 0 || $p99_mad / $p99_median <= $max_p99_mad_ratio)
        && $baseline_ratio >= 0.95 && $baseline_p99_ratio <= 1.10;
    $passed = 0 unless $runtime_pass;
    push @verdict_rows, {
        runtime => $name, rounds => scalar(@r), rps_median => $rps_median,
        rps_cv => $cv, p99_median => $p99_median, p99_mad => $p99_mad,
        socket_errors => $socket_errors, http_errors => $http_errors,
        process_leaks => $leaks, baseline_rps_ratio => $baseline_ratio,
        baseline_p99_ratio => $baseline_p99_ratio,
        passed => $runtime_pass ? JSON::PP::true : JSON::PP::false,
    };
}

my $csv = $output; $csv =~ s/\.json\z/.csv/; $csv .= '.csv' if $csv eq $output;
open my $ofh, '>', $output or die "cannot write $output: $!\n";
print {$ofh} encode_json({ passed => $passed ? JSON::PP::true : JSON::PP::false, max_cv => $max_cv,
    max_p99_mad_ratio => $max_p99_mad_ratio, min_rounds => $min_rounds,
    runtimes => \@verdict_rows }), "\n";
close $ofh;
open my $cfh, '>', $csv or die "cannot write $csv: $!\n";
print {$cfh} "runtime,rounds,rps_median,rps_cv,p99_median,p99_mad,socket_errors,http_errors,process_leaks,baseline_rps_ratio,baseline_p99_ratio,passed\n";
for my $r (@verdict_rows) {
    print {$cfh} join(',', map { $r->{$_} } qw(runtime rounds rps_median rps_cv p99_median p99_mad socket_errors http_errors process_leaks baseline_rps_ratio baseline_p99_ratio passed)), "\n";
}
print encode_json({ passed => $passed ? JSON::PP::true : JSON::PP::false, output => $output, csv => $csv }), "\n";
exit($passed ? 0 : 1);
