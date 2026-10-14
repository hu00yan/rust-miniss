#!/usr/bin/env bash
set -euo pipefail

search_dirs=(src tests)
for dir in examples benches; do
  if [[ -d "$dir" ]]; then
    search_dirs+=("$dir")
  fi
done

if matches=$(rg -n -U --glob '*.rs' '#\[[^]]*\bignore\b[^]]*\]' "${search_dirs[@]}" 2>/dev/null); then
  printf '%s\n' "$matches" >&2
  echo 'ignored tests are not allowed; move the test to an explicit safety gate or make it pass' >&2
  exit 1
fi

echo 'GREEN: no ignored Rust tests found.'
