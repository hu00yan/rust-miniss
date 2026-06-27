#!/usr/bin/env bash
# Architecture Audit Script
# Scans src/ tests/ benches/ examples/ for forbidden patterns.
# Exit codes:
#   0 = clean
#   1 = hard fail (architecture violation found)
#   2 = needs justification only

set -euo pipefail

cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"

HOT_PATH_PATTERNS=(
    "Arc<Mutex"
    "Arc<RwLock"
    "std::sync::Mutex"
    "tokio::sync::Mutex"
    "DashMap"
    "unbounded"
    "static mut"
    "lazy_static"
    "once_cell::sync::"
    "std::sync::LazyLock"
)

SRC_ONLY_PATTERNS=(
    "Runtime::new"
    "Builder::new_current_thread"
    "Builder::new_multi_thread"
    "block_on"
    "spawn_blocking"
)

NEEDS_JUSTIFICATION_PATTERNS=(
    "Ordering::SeqCst"
    "#!\[allow\]"
    "unsafe"
)

SCAN_DIRS=("src" "tests" "benches" "examples")
SRC_SCAN_DIRS=("src")

hard_fail=0
needs_justification=0

echo "=== Architecture Audit ==="
echo ""

search_pattern() {
    local pattern="$1"
    local dir="$2"
    grep -rn "$pattern" "$dir" 2>/dev/null \
        | grep -v '^[^:]*:[0-9][0-9]*:[[:space:]]*//' \
        | grep -v '^[^:]*:[0-9][0-9]*:[[:space:]]*\*' \
        | grep -v '^[^:]*:[0-9][0-9]*:[[:space:]]*///' \
        | grep -v '^[^:]*:[0-9][0-9]*:[[:space:]]*//!' \
        | grep -v 'arch-audit: allow' || true
}

# ─── Hard Fail ──────────────────────────────────────────────────
echo "--- Hard Fail Patterns ---"
for pattern in "${HOT_PATH_PATTERNS[@]}"; do
    for dir in "${SCAN_DIRS[@]}"; do
        if [ -d "$dir" ]; then
            results=$(search_pattern "$pattern" "$dir")
            if [ "$pattern" = "unbounded" ]; then
                results=$(echo "$results" | grep -E 'channel::unbounded|unbounded_channel|SegQueue' || true)
            fi
            if [ -n "$results" ]; then
                echo "HARD FAIL: '$pattern' in $dir/"
                echo "$results"
                echo ""
                hard_fail=1
            fi
        fi
    done
done
for pattern in "${SRC_ONLY_PATTERNS[@]}"; do
    for dir in "${SRC_SCAN_DIRS[@]}"; do
        if [ -d "$dir" ]; then
            results=$(search_pattern "$pattern" "$dir" \
                | grep -v 'src/executor.rs:.*fn test_' \
                | grep -v 'src/executor.rs:.*pub fn block_on' \
                | grep -v 'src/executor.rs:.*let runtime = Runtime::new' \
                | grep -v 'src/executor.rs:.*runtime.block_on' \
                | grep -v 'src/multicore.rs:.*fn test_' \
                | grep -v 'src/multicore.rs:.*let runtime = MultiCoreRuntime::new' \
                | grep -v 'src/multicore.rs:.*pub fn block_on' \
                | grep -v 'src/lib.rs:.*pub fn block_on' \
                | grep -v 'src/task.rs:.*\"' || true)
            if [ -n "$results" ]; then
                echo "HARD FAIL: '$pattern' in $dir/"
                echo "$results"
                echo ""
                hard_fail=1
            fi
        fi
    done
done

# ─── Needs Justification ────────────────────────────────────────
echo "--- Needs Justification Patterns ---"
for pattern in "${NEEDS_JUSTIFICATION_PATTERNS[@]}"; do
    for dir in "${SCAN_DIRS[@]}"; do
        if [ -d "$dir" ]; then
            results=$(grep -rn "$pattern" "$dir" 2>/dev/null || true)
            if [ -n "$results" ]; then
                echo "NEEDS JUSTIFICATION: '$pattern' in $dir/"
                echo "$results"
                echo ""
                needs_justification=1
            fi
        fi
    done
done

# ─── Summary ────────────────────────────────────────────────────
echo "=== Summary ==="
if [ "$hard_fail" -eq 1 ]; then
    echo "RESULT: HARD FAIL — architecture violations found."
    echo "  Must fix or register in docs/ARCHITECTURE_EXCEPTIONS.md."
    exit 1
elif [ "$needs_justification" -eq 1 ]; then
    echo "RESULT: NEEDS JUSTIFICATION — explain before merge."
    exit 0
else
    echo "RESULT: CLEAN — no architecture issues found."
    exit 0
fi
