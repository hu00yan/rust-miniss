#!/usr/bin/env python3
"""Validate README local links and keep its 7/15 matrix sourced from one record."""

from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent
README = ROOT / "README.md"
BASELINE = ROOT / "docs/review/performance-baseline-2026-07-14.md"
MATRIX_START = "## Full runtime shape matrix — 2026-07-15"
SCENARIOS = {
    "keep-alive 8",
    "keep-alive 32",
    "keep-alive 200",
    "keep-alive 800",
    "close 8",
    "close 32",
    "close 800",
}
LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)")


def table_rows(text: str, scenarios: set[str]) -> dict[str, str]:
    rows: dict[str, str] = {}
    for line in text.splitlines():
        if not line.startswith("|"):
            continue
        cells = [cell.strip() for cell in line.strip("|").split("|")]
        if cells and cells[0] in scenarios:
            rows[cells[0]] = line.strip()
    return rows


def main() -> int:
    errors: list[str] = []
    readme_text = README.read_text(encoding="utf-8")
    baseline_text = BASELINE.read_text(encoding="utf-8")

    if MATRIX_START not in baseline_text:
        errors.append(f"canonical matrix heading is missing from {BASELINE.relative_to(ROOT)}")
    else:
        matrix = baseline_text.split(MATRIX_START, 1)[1].split("\n## ", 1)[0]
        expected = table_rows(matrix, SCENARIOS)
        actual = table_rows(readme_text, SCENARIOS)
        if expected.keys() != actual.keys():
            errors.append(
                "README scenario rows differ from the canonical 7/15 matrix: "
                f"expected {sorted(expected)}, found {sorted(actual)}"
            )
        for scenario in sorted(expected.keys() & actual.keys()):
            if expected[scenario] != actual[scenario]:
                errors.append(
                    f"README {scenario!r} row differs from the canonical matrix:\n"
                    f"  expected: {expected[scenario]}\n  found:    {actual[scenario]}"
                )

    for match in LINK.finditer(readme_text):
        destination = match.group(1).strip().split(maxsplit=1)[0].strip("<>")
        parsed = urlsplit(destination)
        if parsed.scheme or destination.startswith("//") or not parsed.path:
            continue
        target = (README.parent / unquote(parsed.path)).resolve()
        if not target.is_relative_to(ROOT) or not target.exists():
            errors.append(f"README link target does not exist: {destination}")

    for required in (
        "docs/review/performance-baseline-2026-07-14.md",
        "docs/review/performance-baseline-2026-10-15-vm.md",
    ):
        if not any(
            unquote(urlsplit(match.group(1).strip().split(maxsplit=1)[0].strip("<>")).path)
            == required
            for match in LINK.finditer(readme_text)
        ):
            errors.append(f"README must link to the canonical evidence: {required}")

    if errors:
        print("README reference check failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("README links and 7/15 baseline rows match their canonical sources.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
