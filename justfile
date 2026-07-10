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

bench-smoke:
    #!/usr/bin/env sh
    set -e
    commit=$(git rev-parse --short HEAD)
    # Baseline: previous commit (or provide explicit baseline)
    baseline=$(git rev-parse --short HEAD~1 2>/dev/null || echo "none")
    echo "Running bench_nop (release, io_uring)..."
    cargo run --release --features io_uring --example bench_nop 2>&1
    # If bench_nop outputs JSON, it will appear above.
    # For now, do a simple NOP throughput check:
    output=$(cargo run --release --features io_uring --example bench_nop 2>/dev/null | tail -1)
    echo '{"bench":"bench_nop","throughput_ops_per_sec":'$(echo "$output" | grep -oP '\d+' || echo 0)',"commit":"'$commit'","baseline_commit":"'$baseline'"}'

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

gate task="":
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
    just bench-smoke || { echo "YELLOW: bench-smoke failed"; exit 1; }; \
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
    cargo test --all-targets

# Check formatting only
fmt:
    cargo fmt --all -- --check

# Clippy only
clippy:
    cargo clippy --all-targets -- -D warnings

# Optional safety-model hooks. These are explicit because Miri/Loom are not
# available on every host; CI or a Linux toolchain can enable them.
miri-safety:
    cargo miri test --all-features --lib cpu
    cargo miri test --all-features --lib executor
    cargo miri test --all-features --lib waker
    cargo miri test --all-features --lib buffer

loom-safety:
    LOOM_MAX_PREEMPTIONS=3 cargo test --all-features --lib loom_
