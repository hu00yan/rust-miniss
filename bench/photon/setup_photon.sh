#!/usr/bin/env bash
# setup_photon.sh — drop the multi-vCPU perf example into a PhotonLibOS
# checkout and build the HTTP perf targets in release mode.
#
# Usage: setup_photon.sh <photon_checkout> [bin_out_dir]
# Idempotent. Does not modify any upstream file except appending a target
# line + a link-fix block to examples/perf/CMakeLists.txt (the added source
# file is ours). The link block is required when PHOTON_BUILD_DEPENDENCIES
# builds a bundled libcurl: its vendored zlib/openssl static libs must be
# appended AFTER libcurl.a on the example link lines.
set -euo pipefail

PHOTON_DIR=${1:?usage: setup_photon.sh <photon_checkout> [bin_out_dir]}
OUT_BIN=${2:-"$PHOTON_DIR/bench-bins"}
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
NPROC=$(nproc 2>/dev/null || echo 2)

cd "$PHOTON_DIR"

# 1. copy the multi-vCPU example
cp -f "$SCRIPT_DIR/http-server-mc.cpp" examples/perf/http/http-server-mc.cpp

# 2. register the target + link-fix block (idempotent; drop any earlier bare
#    target line from a previous script revision so it is not duplicated).
#    NOTE: the perf targets are registered in examples/CMakeLists.txt (there
#    is no examples/perf/CMakeLists.txt upstream).
rm -f examples/perf/CMakeLists.txt   # orphan file created by an earlier revision
sed -i '/^photon_add_example(http-perf-server-mc perf\/http\/http-server-mc.cpp)$/d' \
    examples/CMakeLists.txt
if ! grep -q "PHOTON_BENCH_EXTRA_LIBS" examples/CMakeLists.txt; then
    cat >> examples/CMakeLists.txt <<'EOF'

photon_add_example(http-perf-server-mc perf/http/http-server-mc.cpp)

# --- appended by rust-miniss bench/photon/setup_photon.sh ---
# The bundled curl (PHOTON_BUILD_DEPENDENCIES=ON) needs zlib+openssl at link
# time. Prefer the vendored static libs built under CMAKE_BINARY_DIR; fall
# back to system -l names.
set(PHOTON_BENCH_EXTRA_LIBS "")
if(EXISTS "${CMAKE_BINARY_DIR}/zlib-build/lib/libz.a")
    list(APPEND PHOTON_BENCH_EXTRA_LIBS "${CMAKE_BINARY_DIR}/zlib-build/lib/libz.a")
else()
    list(APPEND PHOTON_BENCH_EXTRA_LIBS "z")
endif()
foreach(_l ssl crypto)
    if(EXISTS "${CMAKE_BINARY_DIR}/openssl-build/lib/lib${_l}.a")
        list(APPEND PHOTON_BENCH_EXTRA_LIBS "${CMAKE_BINARY_DIR}/openssl-build/lib/lib${_l}.a")
    else()
        list(APPEND PHOTON_BENCH_EXTRA_LIBS "${_l}")
    endif()
endforeach()
# Appending to photon_static (not the example targets) is deliberate: CMake
# expands a target's transitive deps AFTER its direct deps, so deps recorded on
# photon_static land after the vendored libcurl.a on every example link line;
# deps appended to the example executables would land before it.
if(TARGET photon_static)
    target_link_libraries(photon_static PRIVATE ${PHOTON_BENCH_EXTRA_LIBS})
endif()
EOF
    echo "registered http-perf-server-mc target + link block"
fi

# 2b. relax photon's -Werror for toolchains newer than what upstream CI uses
#     (warnings-only change, no behavioral effect; no-op when already applied)
sed -i 's/photon_cc_options(-Werror -Wall -Wno-error=pragmas)/photon_cc_options(-Wall -Wno-error=pragmas)/' \
    CMake/photon-helpers.cmake

# 3. configure (only if the build dir does not exist yet)
if [[ ! -f build/CMakeCache.txt ]]; then
    cmake -S . -B build \
        -DCMAKE_BUILD_TYPE=Release \
        -DPHOTON_ENABLE_URING=ON \
        -DPHOTON_BUILD_TESTING=ON \
        -DPHOTON_BUILD_DEPENDENCIES=ON \
        -DCMAKE_CXX_FLAGS="-Wno-error"
fi

# 4. phase 1: build photon_static so vendored zlib/openssl/curl are present
#    (no-op when the build dir is warm)
cmake --build build -j"$NPROC" --target photon_static

# 5. phase 2: re-configure so the appended block can pick up the now-existing
#    vendored lib paths, then build only the HTTP perf targets
cmake -S . -B build >/dev/null
cmake --build build -j"$NPROC" \
    --target http-perf-server http-perf-client http-perf-server-mc

# 6. collect binaries (statically linked against libphoton.a)
mkdir -p "$OUT_BIN"
for b in http-perf-server http-perf-client http-perf-server-mc; do
    cp -f "build/examples-output/$b" "$OUT_BIN/$b"
done
ls -l "$OUT_BIN"
echo "photon perf binaries ready in $OUT_BIN"
