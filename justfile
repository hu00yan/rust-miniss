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
    cargo clippy --all-targets -- -D warnings
    cargo test --tests --examples

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
    cargo run --release --features io_uring --example bench_nop

# Run HTTP echo benchmark
bench-http:
    ./bench/scripts/bench_http.sh

# Run all tests
test:
    # Do not start benchmark servers under the default test recipe.
    cargo test --tests --examples

# Fast local safety loop. Deliberately excludes Loom, Miri, kernel I/O tests,
# ignored stress tests, examples, and benchmarks.
test-fast:
    cargo fmt --all -- --check
    ./scripts/audit_architecture.sh
    cargo test --lib --test runtime_safety_models

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
    cargo +nightly miri test --lib cpu::tests::runtime_context_guard_restores_timer_task_and_core_on_nesting -- --exact
    cargo +nightly miri test --lib executor::tests::tls_context_restores_after_nesting_and_panic -- --exact
    cargo +nightly miri test --lib waker::tests::test_waker_clone -- --exact
    cargo +nightly miri test --lib waker::tests::test_waker_wake_by_ref -- --exact
    cargo +nightly miri test --lib waker::tests::test_waker_drop_after_clone -- --exact
    cargo +nightly miri test --lib waker::tests::escaped_waker_wakes_from_other_thread_and_late_wake_is_harmless -- --exact
    cargo +nightly miri test --lib buffer::tests::safe_read_destination_is_initialized_before_slice_exposure -- --exact
    cargo +nightly miri test --lib buffer::tests::read_length_outside_capacity_is_rejected_without_publishing -- --exact

loom:
    #!/usr/bin/env sh
    echo "loom safety coverage is not implemented: production synchronization seams have not been extracted" >&2
    echo "missing protocol 1: wake concurrent with task close or slot retirement" >&2
    echo "missing protocol 2: cancellation concurrent with waiter registration" >&2
    echo "missing protocol 3: notification concurrent with worker sleep transition" >&2
    echo "independent reference models are not accepted as evidence for these production protocols" >&2
    exit 1

# Real Linux/io_uring coverage. This is intentionally separate from test-fast.
# These tests use sockets, descriptors, the kernel driver, and (for delayed
# accept) a real thread/timer; model tests do not cover those obligations.
linux-safety:
    sh scripts/linux_safety.sh
