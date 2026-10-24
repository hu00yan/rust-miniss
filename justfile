# rust-miniss justfile — gate definitions
#
# 所有 gate 串成 `just gate` 作为提交前检查。
# 返回码：
#   0 = GREEN (all pass)
#   1 = YELLOW (compile/test/clippy failure)
#   2 = RED (architecture violation)
#   3 = RED (diff out of bounds)

# ─── Basic Check ────────────────────────────────────────────────

check:
    cargo fmt --all -- --check
    ./scripts/check_no_ignored_tests.sh
    cargo clippy --all-targets -- -D warnings
    cargo check --all-targets --all-features
    # Include regular library/integration/example tests; long stress cases
    # have their own explicit gate below.
    just test-regular

# ─── Documentation Audit ────────────────────────────────────────

docs-audit:
    python3 scripts/check_readme_references.py

# ─── Architecture Audit ────────────────────────────────────────

arch-audit:
    ./scripts/audit_architecture.sh

# ─── Diff Audit ─────────────────────────────────────────────────

diff-audit task="":
    #!/usr/bin/env sh
    set -e
    if [ -z "{{task}}" ]; then
        echo "Usage: just diff-audit TASK=<path/to/task.yaml>"
        exit 1
    fi
    # Extract allowed_files from task yaml
    allowed=$(grep -A100 '^allowed_files:' "{{task}}" | sed -n '/^  - /s/^  - //p')
    changed=$(git diff --name-only --cached)
    for f in $changed; do
        found=0
        for a in $allowed; do
            case "$f" in
                "$a"|"$a"/*) found=1; break ;;
            esac
        done
        if [ "$found" = "0" ]; then
            echo "RED: Changed file outside allowed_files: $f"
            echo "  Allowed:"
            for a in $allowed; do echo "    - $a"; done
            exit 3
        fi
    done
    echo "GREEN: All changed files are within allowed set."

# ─── Benchmark Smoke ────────────────────────────────────────────

bench-smoke baseline="":
    #!/usr/bin/env sh
    set -eu
    if [ -z "{{ baseline }}" ]; then
        echo "Usage: just bench-smoke BASELINE=<summary.csv>" >&2
        echo "A real same-host baseline is required; HEAD~1 is not benchmark evidence." >&2
        exit 1
    fi
    baseline_metadata=$(dirname "{{ baseline }}")/metadata.json
    if [ ! -f "$baseline_metadata" ]; then
        echo "Missing baseline metadata: $baseline_metadata" >&2
        echo "Use summary.csv and metadata.json produced by bench_http.sh together." >&2
        exit 1
    fi
    commit=$(git rev-parse --short HEAD)
    out=".scratch/bench-smoke-$commit-$(date +%Y%m%d-%H%M%S)"
    HTTP_CORES=1 BENCH_RUNTIMES=miniss BENCH_OUT_DIR="$out" ./bench/scripts/bench_http.sh 9090 5 32 1
    perl bench/scripts/compare_http_baseline.pl "{{ baseline }}" "$out/summary.csv" "$commit"

# ─── Review ─────────────────────────────────────────────────────

review:
    @echo "Run review agent manually:"
    @echo "  just review-diff   — standard review"
    @echo "  just review-adversarial — adversarial review"
    @echo ""
    @echo "Both review agents read git diff and output findings with file+line."
    @echo "See AGENTS.md -> Gate Definitions -> just review for full spec."

review-diff:
    @echo "=== Standard Review ==="
    @echo "Reviewing git diff..."
    @git --no-pager diff --cached -U10 2>/dev/null || git --no-pager diff -U10
    @echo ""
    @echo "Checklist:"
    @echo "  1. Contract violations (MUST/MUST NOT)"
    @echo "  2. Missing evidence (tests, benchmarks, comments)"
    @echo "  3. Required blocking changes"
    @echo "Each finding MUST cite file:line."

review-adversarial:
    @echo "=== Adversarial Review ==="
    @echo "Assume this patch is subtly wrong. Check for:"
    @echo "  - Hidden cross-core lock contention"
    @echo "  - Thread-local ownership broken"
    @echo "  - Runtime creation hidden in helpers"
    @echo "  - Benchmark data manipulation"
    @echo "  - Tail latency regression"
    @echo "  - Tests pass but production behavior regresses"
    @echo "Each finding MUST cite code evidence."

# ─── Gate (full sequence) ───────────────────────────────────────

gate task="" baseline="":
    @set -e; \
    echo "=== Gate: check ==="; \
    just check || { echo "YELLOW: check failed"; exit 1; }; \
    echo "=== Gate: arch-audit ==="; \
    just arch-audit || { echo "RED: architecture violation"; exit 2; }; \
    echo "=== Gate: unsafe-audit ==="; \
    just unsafe-audit || { echo "RED: unsafe code lacks a safety explanation"; exit 2; }; \
    if [ -n "{{task}}" ]; then \
        echo "=== Gate: diff-audit ==="; \
        just diff-audit {{task}} || { echo "RED: out-of-bounds file change"; exit 3; }; \
    fi; \
    echo "=== Gate: bench-smoke ==="; \
    just bench-smoke "{{baseline}}" || { echo "YELLOW: bench-smoke failed"; exit 1; }; \
    echo ""; \
    echo "=== GATE PASSED (GREEN) ==="

# ─── Utility ────────────────────────────────────────────────────

# Run bench_nop manually
bench-nop:
    cargo run --release --example bench_nop

# Run HTTP echo benchmark
bench-http:
    @echo "Optional Linux placement: BENCH_SERVER_CPUS=0-7 BENCH_CLIENT_CPUS=8-9 HTTP_CORES=8 just bench-http"
    @echo "Unset placement is recorded as unbound and is only a contention diagnostic."
    ./bench/scripts/bench_http.sh

# Explicit multi-round HTTP/runtime matrix. Set BENCH_SERVER_CPUS,
# BENCH_CLIENT_CPUS, and HTTP_CORES on Linux when collecting isolated evidence.
# Every round is retained; this recipe is intentionally outside the default gate.
bench-matrix rounds="1" scenarios="":
    BENCH_MATRIX_ROUNDS="{{rounds}}" HTTP_MATRIX_SCENARIOS="{{scenarios}}" \
        ./bench/scripts/bench_http_matrix.sh

# Explicit worker/client-thread topology matrix. Requires non-overlapping
# BENCH_SERVER_CPUS and BENCH_CLIENT_CPUS; use BENCH_TOPOLOGY_* to narrow the
# default 1/2/4/8 worker x 1/2/4 client-thread matrix.
bench-topology rounds="1":
    BENCH_MATRIX_ROUNDS="{{rounds}}" ./bench/scripts/bench_http_topology.sh

# Evaluate one or more completed bench_http summaries mechanically. Set
# BENCH_BASELINE to a same-shape summary.csv to enforce the 5% throughput
# regression threshold. The command emits adjacent JSON and CSV verdicts and
# returns non-zero on errors, leaked processes, excessive CV, too few rounds,
# or baseline regression.
bench-verdict output summaries:
    BENCH_BASELINE="${BENCH_BASELINE:-}" perl bench/scripts/bench_verdict.pl "{{output}}" {{summaries}}

# Run all tests
test:
    # Do not start benchmark servers under the default test recipe.
    ./scripts/check_no_ignored_tests.sh
    just test-regular

# Regular correctness tests. Stress and long-running lifecycle tests are run
# once by `just stress`, rather than once per feature combination.
test-regular:
    cargo test --lib --tests --examples -- --test-threads=1 \
        --skip test_long_running \
        --skip test_udp_stress \
        --skip test_udp_stress_multiple_senders \
        --skip test_multicore_sustained_admission_pressure \
        --skip test_multicore_loaded_shutdown_reclaims_all_state \
        --skip test_multicore_loaded_shutdown_worker_and_task_matrix \
        --skip test_multicore_pending_udp_shutdown_reclaims_socket_leases \
        --skip test_multicore_udp_receive_close_race_reclaims_state \
        --skip test_multicore_accepted_tcp_shutdown_reclaims_descriptor_leases \
        --skip stress_timer_io

# Bounded multi-core pressure loop. The default tests stay short; this explicit
# recipe runs the same stress target for 30 seconds and verifies cleanup.
stress:
    ./scripts/check_no_ignored_tests.sh
    MINISS_STRESS_DURATION_MS=30000 MINISS_SHUTDOWN_STRESS_DURATION_MS=10000 \
        MINISS_SHUTDOWN_STRESS_ROUNDS=3 \
        MINISS_MATRIX_STRESS_DURATION_MS=3000 \
        MINISS_RETAINED_FD_ROUNDS=2 \
        MINISS_TCP_RETAINED_FD_ROUNDS=2 \
        cargo test --test stress_test -- --test-threads=1

# Protocol/feature-complete test loop. This is explicit because it enables TLS,
# QUIC/H3, and every optional backend feature; it is slower than the default
# check and still excludes benchmark binaries.
test-all-features:
    cargo fmt --all -- --check
    ./scripts/check_no_ignored_tests.sh
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-features --lib --tests --examples -- --test-threads=1 \
        --skip test_long_running \
        --skip test_udp_stress \
        --skip test_udp_stress_multiple_senders \
        --skip test_multicore_sustained_admission_pressure \
        --skip test_multicore_loaded_shutdown_reclaims_all_state \
        --skip test_multicore_loaded_shutdown_worker_and_task_matrix \
        --skip test_multicore_pending_udp_shutdown_reclaims_socket_leases \
        --skip test_multicore_udp_receive_close_race_reclaims_state \
        --skip test_multicore_accepted_tcp_shutdown_reclaims_descriptor_leases \
        --skip stress_timer_io

# Exercise feature-off and feature-on compilation/test surfaces. Benchmark
# binaries remain excluded because they launch live workloads. Each matrix
# entry is serial so stress cases cannot contend with one another.
#
# `arena` / `rent-traits` / `advanced` were removed: they gated nothing
# (`rent-traits`) or only verbatim-duplicated buffer constructors
# (`arena` / `advanced`). `arena` was also incompatible with `Buffer`'s
# owned-`Vec` + pooled-recycle model.
test-feature-matrix:
    ./scripts/check_no_ignored_tests.sh
    cargo check --no-default-features --all-targets
    cargo check --no-default-features --features tls --all-targets
    cargo check --no-default-features --features signal --all-targets
    cargo check --no-default-features --features debug --all-targets
    just test-all-features

# Run the complete correctness suite without starting benchmark binaries.
# Kernel- and toolchain-specific gates are included when the host can run them;
# skipped gates are reported explicitly instead of looking green.
test-full:
    #!/usr/bin/env sh
    set -eu
    just check
    just test-feature-matrix
    just test-differential
    just loom
    just stress
    if [ "$(uname -s)" = "Linux" ]; then
        just linux-safety
    else
        echo "SKIP: linux-safety requires Linux io_uring"
    fi
    if rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
        if rustup run nightly cargo miri --version >/dev/null 2>&1; then
            just miri-safety
        else
            echo "SKIP: nightly exists but cargo-miri is unavailable"
        fi
    else
        echo "SKIP: nightly toolchain is unavailable for miri-safety"
    fi

# Reference-model and production-linked differential tests. The current slice
# covers generational TaskArena behavior; operation/kernel protocols remain
# split between pure models and backend-specific regression tests until a
# deterministic fake backend is available.
test-differential:
    cargo test --test runtime_safety_models

# Fast local safety loop. It excludes Loom, Miri, the separate long Linux
# lifetime/stress gates, examples, and benchmarks. `runtime_regressions` still
# includes bounded I/O tests against the backend selected for this host.
test-fast:
    cargo fmt --all -- --check
    ./scripts/check_no_ignored_tests.sh
    ./scripts/audit_architecture.sh
    cargo test --lib --test runtime_safety_models
    cargo test --test runtime_regressions

# Explicit unsafe documentation gate. It is included in `just gate`, but not
# the fast local loop until the existing baseline is fully documented.
unsafe-audit:
    ./scripts/check_unsafe_safety.sh

# Check formatting only
fmt:
    cargo fmt --all -- --check

# Clippy only
clippy:
    cargo clippy --all-targets -- -D warnings

# Optional safety-model hooks. These are explicit because Miri/Loom are not
# available on every host; CI or a Linux toolchain can enable them.
miri-safety:
    # Runtime executor tests that construct real backends are excluded: Miri
    # cannot emulate io_uring syscalls. These exact tests cover TLS/identity.
    ./scripts/run_exact_test.sh rustup run nightly cargo miri test --lib cpu::tests::runtime_context_guard_restores_timer_task_and_core_on_nesting -- --exact
    ./scripts/run_exact_test.sh rustup run nightly cargo miri test --lib executor::tests::tls_context_restores_after_nesting_and_panic -- --exact
    ./scripts/run_exact_test.sh rustup run nightly cargo miri test --lib waker::tests::test_waker_clone -- --exact
    ./scripts/run_exact_test.sh rustup run nightly cargo miri test --lib waker::tests::test_waker_wake_by_ref -- --exact
    ./scripts/run_exact_test.sh rustup run nightly cargo miri test --lib waker::tests::test_waker_drop_after_clone -- --exact
    ./scripts/run_exact_test.sh rustup run nightly cargo miri test --lib waker::tests::escaped_waker_wakes_from_other_thread_and_late_wake_is_harmless -- --exact
    ./scripts/run_exact_test.sh rustup run nightly cargo miri test --lib buffer::tests::safe_read_destination_is_initialized_before_slice_exposure -- --exact
    ./scripts/run_exact_test.sh rustup run nightly cargo miri test --lib buffer::tests::read_length_outside_capacity_is_rejected_without_publishing -- --exact

no-ignored-tests:
    ./scripts/check_no_ignored_tests.sh

loom:
    MINISS_LOOM=1 ./scripts/run_exact_test.sh cargo test --lib cancellation::tests::loom_cancel_races_with_waiter_registration -- --exact --nocapture
    MINISS_LOOM=1 ./scripts/run_exact_test.sh cargo test --lib multicore::tests::loom_worker_sleep_gate_cannot_lose_notification -- --exact --nocapture
    MINISS_LOOM=1 ./scripts/run_exact_test.sh cargo test --lib waker::tests::loom_wake_racing_with_deactivate_cannot_rearm_closed_state -- --exact --nocapture

# Real Linux/io_uring coverage. This is intentionally separate from test-fast.
# These tests use sockets, descriptors, the kernel driver, and (for delayed
# accept) a real thread/timer; model tests do not cover those obligations.
linux-safety:
    sh scripts/linux_safety.sh
