#!/usr/bin/env perl
use strict;
use warnings;
use File::Basename qw(dirname);
use File::Spec;
use JSON::PP qw(decode_json encode_json);
use Text::ParseWords qw(parse_line);

my ($baseline_path, $current_path, $commit) = @ARGV;
die "usage: compare_http_baseline.pl BASELINE.csv CURRENT.csv COMMIT\n"
    unless defined $commit;

sub read_row {
    my ($path) = @_;
    open my $fh, '<', $path or die "cannot read $path: $!\n";
    chomp(my $header = <$fh> // die "empty csv: $path\n");
    my @keys = parse_line(',', 0, $header);
    while (my $line = <$fh>) {
        chomp $line;
        next unless length $line;
        my @values = parse_line(',', 0, $line);
        my %row;
        @row{@keys} = @values;
        return \%row if ($row{name} // '') eq 'miniss';
    }
    die "missing miniss row in $path\n";
}

sub read_metadata {
    my ($summary_path) = @_;
    my $path = File::Spec->catfile(dirname($summary_path), 'metadata.json');
    open my $fh, '<', $path or die "missing benchmark metadata $path: $!\n";
    local $/;
    my $metadata = eval { decode_json(<$fh>) };
    die "invalid benchmark metadata $path: $@" if $@;
    return ($path, $metadata);
}

sub latency_us {
    my ($value) = @_;
    $value =~ /^([0-9]+(?:\.[0-9]+)?)(ns|us|ms|s)$/
        or die "unsupported latency value: $value\n";
    my ($number, $unit) = ($1 + 0, $2);
    return $number / 1000 if $unit eq 'ns';
    return $number if $unit eq 'us';
    return $number * 1000 if $unit eq 'ms';
    return $number * 1_000_000;
}

my $baseline = read_row($baseline_path);
my $current = read_row($current_path);
my ($baseline_metadata_path, $baseline_metadata) = read_metadata($baseline_path);
my ($current_metadata_path, $current_metadata) = read_metadata($current_path);
my @metadata_fields = qw(host os kernel cpu_model cores threads connections duration runtime server_cpus client_cpus affinity_mode);
for my $field (@metadata_fields) {
    die "missing metadata field '$field' in $baseline_metadata_path\n"
        unless defined $baseline_metadata->{$field};
    die "missing metadata field '$field' in $current_metadata_path\n"
        unless defined $current_metadata->{$field};
    die "benchmark metadata mismatch for '$field': baseline='$baseline_metadata->{$field}' current='$current_metadata->{$field}'\n"
        unless "$baseline_metadata->{$field}" eq "$current_metadata->{$field}";
}
my $baseline_rps = 0 + $baseline->{rps};
my $current_rps = 0 + $current->{rps};
my $baseline_p99 = latency_us($baseline->{p99});
my $current_p99 = latency_us($current->{p99});

my $throughput_ratio = $baseline_rps ? $current_rps / $baseline_rps : 0;
my $p99_ratio = $baseline_p99 ? $current_p99 / $baseline_p99 : 0;
my $passed = $throughput_ratio >= 0.95 && $p99_ratio <= 1.10;

print encode_json({
    bench => 'http-smoke',
    commit => $commit,
    baseline_file => $baseline_path,
    baseline_metadata_file => $baseline_metadata_path,
    current_metadata_file => $current_metadata_path,
    throughput_ops_per_sec => $current_rps,
    baseline_throughput_ops_per_sec => $baseline_rps,
    throughput_ratio => $throughput_ratio,
    p99_us => $current_p99,
    baseline_p99_us => $baseline_p99,
    p99_ratio => $p99_ratio,
    passed => $passed ? JSON::PP::true : JSON::PP::false,
}), "\n";

exit($passed ? 0 : 1);
