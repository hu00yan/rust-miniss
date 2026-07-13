#!/usr/bin/env bash
# Enforce safety explanations for unsafe blocks and functions.
set -euo pipefail

cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)"
echo "=== Unsafe safety audit ==="
echo "Every unsafe block must have an immediately preceding // SAFETY: comment."
echo "Every unsafe fn must document caller obligations in a # Safety section."

# Reuse the repository scanner as the human-readable inventory.  It currently
# reports legacy blocks that have a nearby (but not immediately preceding)
# explanation; the strict clippy check below is what turns that inventory into
# a blocking verdict.
audit_output="$(./scripts/audit_architecture.sh || true)"
if grep -q "NEEDS JUSTIFICATION: 'unsafe'" <<<"$audit_output"; then
    echo "$audit_output"
else
    echo "$audit_output"
fi

# Keep this gate focused on safety documentation; ordinary clippy policy is
# enforced separately by `just check`.
set +e
cargo clippy --all-targets --all-features -- -A warnings -D clippy::undocumented_unsafe_blocks
clippy_rc=$?
set -e

set +e
python3 - <<'PY'
import pathlib
import re
import sys

missing = []
for root in (pathlib.Path("src"), pathlib.Path("tests"), pathlib.Path("examples")):
    if not root.exists():
        continue
    for path in sorted(root.rglob("*.rs")):
        lines = path.read_text(errors="replace").splitlines()
        for index, line in enumerate(lines):
            if not re.search(r"\bunsafe\s+(?:pub(?:\([^)]*\))?\s+)?fn\b", line):
                continue
            docs = "\n".join(lines[max(0, index - 20):index])
            if "# Safety" not in docs:
                missing.append(f"{path}:{index + 1}: unsafe fn is missing a # Safety section")

if missing:
    print("unsafe functions missing # Safety documentation:", file=sys.stderr)
    print("\n".join(missing), file=sys.stderr)
    sys.exit(1)
PY
python_rc=$?
set -e
if [ "$clippy_rc" -ne 0 ] || [ "$python_rc" -ne 0 ]; then
    echo "RESULT: FAIL — unsafe safety explanations are missing." >&2
    exit 1
fi

echo "RESULT: CLEAN — unsafe blocks and unsafe function contracts are documented."
