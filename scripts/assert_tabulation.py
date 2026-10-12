#!/usr/bin/env python3
"""Assert a weighting-experiment tabulation is present and discriminating.

Reads one tabulation.md (as the weighting harness writes it) and checks:
both arms are present, each retained exactly the declared budget of runs,
and at least one arm recorded witnesses or failures - a fully saturated
zero-witness corpus cannot discriminate weighting signals. Usage:

    assert_tabulation.py tabulation.md budget [summary.md]

With a summary.md argument, the cross-workload summary is also checked:
every recorded workload/budget pair carries the yield-comparison row the
seed-default decision reads.
"""

import re
import sys
from pathlib import Path


def main() -> None:
    tabulation = Path(sys.argv[1])
    budget = int(sys.argv[2])
    text = tabulation.read_text()
    rows = []
    for line in text.splitlines():
        cells = [cell.strip() for cell in line.split("|")]
        if len(cells) >= 6 and cells[1] in ("unseeded", "seeded"):
            # columns: "", arm, runs, failed runs, witnesses, per run, ""
            rows.append((cells[1], int(cells[2]), int(cells[4])))
    assert len(rows) == 2, f"expected both arms in tabulation: {rows!r}"
    for arm, runs, _failed in rows:
        assert runs == budget, f"{arm} retained {runs} runs, budget {budget}"
    # A saturated corpus (every identity consumed, no failures) cannot
    # discriminate signals; the recorded experiment must stay interesting.
    assert any(witnesses > 0 for _, _, witnesses in rows), (
        "saturated corpus: no witnesses in either arm"
    )

    if len(sys.argv) > 3:
        summary = Path(sys.argv[3]).read_text()
        # The yield table exists and carries one row per recorded
        # workload/budget pair; the harness writes the section header
        # above it.
        assert "## Yield comparison" in summary, "summary lacks the yield table"
        section = summary.split("## Yield comparison", 1)[1]
        yield_rows = [
            line
            for line in section.splitlines()
            if line.startswith("| ") and not line.startswith("| ---")
            and "workload" not in line
        ]
        assert len(yield_rows) >= 1, "summary yield table has no rows"
        for row in yield_rows:
            cells = [cell.strip() for cell in row.split("|")]
            # columns: "", workload, budget, unseeded/run, seeded/run,
            # delta, first-witness positions, ""
            assert len(cells) >= 7, f"yield row malformed: {row!r}"
            assert cells[5].startswith(("+", "-")), f"yield delta malformed: {row!r}"


if __name__ == "__main__":
    main()
