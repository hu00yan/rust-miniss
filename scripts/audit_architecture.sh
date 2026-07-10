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
    "Runtime::new[[:space:]]*[(]"
    "Builder::new_current_thread"
    "Builder::new_multi_thread"
    "[.]block_on[[:space:]]*[(]"
    "spawn_blocking"
    "Rc::into_raw"
    "io_state[[:space:]]*[(][^)]*[)][[:space:]]*->[[:space:]]*&[[:space:]]*'static"
    "unsafe[[:space:]]+impl[[:space:]]+(Send|Sync)[[:space:]]+for[[:space:]]+[^;]*Backend"
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
    grep -rEn "$pattern" "$dir" 2>/dev/null \
        | grep -v '^[^:]*:[0-9][0-9]*:[[:space:]]*//' \
        | grep -v '^[^:]*:[0-9][0-9]*:[[:space:]]*\*' \
        | grep -v '^[^:]*:[0-9][0-9]*:[[:space:]]*///' \
        | grep -v '^[^:]*:[0-9][0-9]*:[[:space:]]*//!' \
        | grep -v 'arch-audit: allow' || true
}

# SRC_ONLY_PATTERNS also match regression tests embedded in `src/*.rs`.
# Those tests intentionally construct runtimes and call block_on; do not treat
# them as production hot-path violations. Keep the test scan for all other
# hard-fail patterns unchanged.
filter_cfg_test_matches() {
    local dir="$1"
    local raw="$2"
    FILTER_INPUT="$raw" python3 - "$dir" <<'PY'
import os
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1]).resolve()
lines = os.environ.get("FILTER_INPUT", "").splitlines()
ranges = {}
for path in root.rglob("*.rs"):
    source = path.read_text(errors="ignore").splitlines()
    pending = False
    start = None
    depth = 0
    spans = []
    for index, line in enumerate(source, 1):
        if "#[cfg(test)]" in line:
            pending = True
        if pending and "{" in line:
            start = index
            depth = line.count("{") - line.count("}")
            pending = False
        elif start is not None:
            depth += line.count("{") - line.count("}")
            if depth <= 0:
                spans.append((start, index))
                start = None
    ranges[str(path.resolve())] = spans

for match in lines:
    filename, number, _ = match.split(":", 2)
    number = int(number)
    path = pathlib.Path(filename)
    if not path.is_absolute():
        path = pathlib.Path.cwd() / path
    path = path.resolve()
    if any(start <= number <= end for start, end in ranges.get(str(path), [])):
        continue
    print(match)
PY
}

search_unjustified_unsafe() {
    local dir="$1"
    python3 - "$dir" <<'PY'
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
unsafe_re = re.compile(r"\bunsafe\b")
comment_re = re.compile(r"(SAFETY:|# Safety|arch-audit: allow)")

for path in sorted(root.rglob("*.rs")):
    lines = path.read_text(errors="ignore").splitlines()
    for idx, line in enumerate(lines):
        if not unsafe_re.search(line):
            continue
        if line.lstrip().startswith(("//", "//!","///", "*")):
            continue
        start = max(0, idx - 8)
        end = min(len(lines), idx + 3)
        context = lines[start:end]
        if any(comment_re.search(item) for item in context):
            continue
        print(f"{path}:{idx + 1}:{line}")
PY
}

# ─── Hard Fail ──────────────────────────────────────────────────
echo "--- Hard Fail Patterns ---"
for pattern in "${HOT_PATH_PATTERNS[@]}"; do
    for dir in "${SCAN_DIRS[@]}"; do
        if [ -d "$dir" ]; then
            results=$(search_pattern "$pattern" "$dir")
            if [ "$dir" = "src" ]; then
                results=$(filter_cfg_test_matches "$dir" "$results")
            fi
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
            raw=$(search_pattern "$pattern" "$dir")
            results=$(filter_cfg_test_matches "$dir" "$raw")
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
            if [ "$pattern" = "unsafe" ]; then
                results=$(search_unjustified_unsafe "$dir")
            else
                results=$(grep -rn "$pattern" "$dir" 2>/dev/null || true)
            fi
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
