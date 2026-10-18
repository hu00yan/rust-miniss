#!/usr/bin/env bash
set -euo pipefail

addr="${PROMETHEUS_TEST_ADDR:-127.0.0.1:19193}"
url="http://${addr}/metrics"
mkdir -p .scratch
workdir="$(mktemp -d .scratch/prometheus-check.XXXXXX)"
server_pid=""

cleanup() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$workdir"
}
trap cleanup EXIT

cargo build --locked --example metrics_export
"target/debug/examples/metrics_export" "$addr" >"$workdir/server.log" 2>&1 &
server_pid=$!

ready=0
for _ in $(seq 1 50); do
  if curl --silent --show-error --fail --max-time 0.25 \
      --dump-header "$workdir/headers" --output "$workdir/metrics.txt" "$url" 2>/dev/null; then
    ready=1
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    cat "$workdir/server.log" >&2
    echo "metrics exporter exited before serving $url" >&2
    exit 1
  fi
  sleep 0.05
done

if [[ "$ready" != 1 ]]; then
  cat "$workdir/server.log" >&2
  echo "metrics exporter did not serve $url within the bounded readiness window" >&2
  exit 1
fi

if [[ ! -s "$workdir/metrics.txt" ]]; then
  echo "metrics endpoint returned an empty body" >&2
  exit 1
fi

if ! tr -d '\r' <"$workdir/headers" | grep -Eiq '^content-type: text/plain; version=0\.0\.4'; then
  echo "metrics endpoint did not return Prometheus text exposition content type" >&2
  cat "$workdir/headers" >&2
  exit 1
fi

docker run --rm -i --entrypoint=/bin/promtool prom/prometheus:v3.5.0 \
  check metrics <"$workdir/metrics.txt"

wait "$server_pid"
server_pid=""
echo "Live /metrics scrape passed promtool v3.5.0."
