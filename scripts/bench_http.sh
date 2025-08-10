#!/usr/bin/env bash
set -euo pipefail

# Ensure wrk and hyperfine are available
if ! command -v wrk >/dev/null 2>&1; then
  echo "wrk not found. Please install wrk (e.g., brew install wrk)."
  exit 1
fi
if ! command -v hyperfine >/dev/null 2>&1; then
  echo "hyperfine not found. Please install hyperfine (e.g., brew install hyperfine)."
  exit 1
fi

PORT=${PORT:-8080}
ADDR="127.0.0.1:${PORT}"
DURATION=${DURATION:-30s}
THREADS=${THREADS:-4}
CONNECTIONS=${CONNECTIONS:-256}

# Build release binaries
cargo build --release --examples

# Run server in background
./target/release/examples/http_echo --addr ${ADDR} &
SERVER_PID=$!
trap 'kill ${SERVER_PID} >/dev/null 2>&1 || true' EXIT

# Wait for server to be ready
for i in {1..50}; do
  if curl -sSf "http://${ADDR}/" >/dev/null; then
    break
  fi
  sleep 0.1
done

# Run wrk benchmark
echo "Running wrk against http://${ADDR}/"
WRK_OUT=$(wrk -t${THREADS} -c${CONNECTIONS} -d${DURATION} http://${ADDR}/)

REQ_PER_SEC=$(echo "$WRK_OUT" | grep -Eo "Requests/sec:[[:space:]]+[0-9.]+" | awk '{print $2}')
P99=$(echo "$WRK_OUT" | awk '/Latency Distribution/{flag=1;next}/[0-9]+\.[0-9]+%/{if($1=="99.000%") print $2}' | tr -d 'ms')

echo "Requests/sec: ${REQ_PER_SEC}"
if [ -n "${P99}" ]; then
  echo "p99 latency: ${P99} ms"
else
  echo "p99 latency: N/A (wrk output format changed?)"
fi

# Build tokio version
cargo build --release --bin http_echo_tokio 2>/dev/null || true

# Hyperfine comparison
# If binaries exist, use them; otherwise, fall back to examples path for the first
B1="./target/release/examples/http_echo"
B2="./target/release/http_echo_tokio"

CMD1="$B1 --addr ${ADDR}"
CMD2="$B2 --addr ${ADDR}"

if [ ! -x "$B2" ]; then
  echo "Tokio binary not found (expected $B2). Skipping hyperfine."
else
  echo "Running hyperfine comparison..."
  hyperfine "${CMD1}" "${CMD2}" --warmup 1 --time-unit millisecond --export-markdown /tmp/hf_http.md || true
  echo "Hyperfine results saved to /tmp/hf_http.md"
fi
