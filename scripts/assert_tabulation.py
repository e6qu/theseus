#!/usr/bin/env python3
"""Assert a weighting-experiment tabulation is present and discriminating.

Reads one tabulation.md (as the weighting harness writes it) and checks:
both arms are present, each retained exactly the declared budget of runs,
and at least one arm recorded witnesses or failures - a fully saturated
zero-witness corpus cannot discriminate weighting signals. Usage:

    assert_tabulation.py tabulation.md budget
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


if __name__ == "__main__":
    main()
