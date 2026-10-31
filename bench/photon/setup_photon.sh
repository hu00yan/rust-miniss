#!/usr/bin/env bash
# setup_photon.sh — drop the multi-vCPU perf example into a PhotonLibOS
# checkout and build the HTTP perf targets in release mode.
#
# Usage: setup_photon.sh <photon_checkout> [bin_out_dir]
# Idempotent. Does not modify any upstream file except appending one target
# line to examples/perf/CMakeLists.txt (the added source file is ours).
set -euo pipefail

PHOTON_DIR=${1:?usage: setup_photon.sh <photon_checkout> [bin_out_dir]}
OUT_BIN=${2:-"$PHOTON_DIR/bench-bins"}
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
NPROC=$(nproc 2>/dev/null || echo 2)

cd "$PHOTON_DIR"

# 1. copy the multi-vCPU example
cp -f "$SCRIPT_DIR/http-server-mc.cpp" examples/perf/http/http-server-mc.cpp

# 2. register the target (idempotent)
if ! grep -q "http-perf-server-mc" examples/perf/CMakeLists.txt; then
    printf '\nphoton_add_example(http-perf-server-mc perf/http/http-server-mc.cpp)\n' \
        >> examples/perf/CMakeLists.txt
    echo "registered http-perf-server-mc target"
fi

# 3. configure (only if the build dir does not exist yet)
if [[ ! -f build/CMakeCache.txt ]]; then
    cmake -S . -B build \
        -DCMAKE_BUILD_TYPE=Release \
        -DPHOTON_ENABLE_URING=ON \
        -DPHOTON_BUILD_TESTING=OFF
fi

# 4. build only the HTTP perf targets
cmake --build build -j"$NPROC" \
    --target http-perf-server http-perf-client http-perf-server-mc

# 5. collect binaries (statically linked against libphoton.a)
mkdir -p "$OUT_BIN"
for b in http-perf-server http-perf-client http-perf-server-mc; do
    cp -f "build/examples-output/$b" "$OUT_BIN/$b"
done
ls -l "$OUT_BIN"
echo "photon perf binaries ready in $OUT_BIN"
