#!/usr/bin/env bash
set -euo pipefail

if (($# == 0)); then
  echo 'usage: run_exact_test.sh <test command and arguments>' >&2
  exit 2
fi

output=$("$@" 2>&1) || {
  printf '%s\n' "$output"
  exit 1
}
printf '%s\n' "$output"

if ! grep -Eq 'test result:.*[1-9][0-9]* passed' <<<"$output"; then
  echo 'exact test command ran zero tests' >&2
  exit 1
fi
