#!/usr/bin/env python3
"""Require every workflow action dependency to name an immutable commit."""

from __future__ import annotations

from pathlib import Path
import re


WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"
ACTION = re.compile(r"^\s*(?:-\s*)?uses:\s*[^@\s]+@([0-9a-f]{40})\s*$", re.MULTILINE)
USES = re.compile(r"^\s*(?:-\s*)?uses:\s*", re.MULTILINE)


def main() -> None:
    for workflow in sorted(WORKFLOWS.glob("*.yml")):
        contents = workflow.read_text()
        assert len(ACTION.findall(contents)) == len(USES.findall(contents)), workflow


if __name__ == "__main__":
    main()
