#!/usr/bin/env python3
"""Check that tutorial instructions remain visible and self-contained."""

from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[2]
TUTORIALS = ROOT / "docs" / "tutorials"
HARNESS_TUTORIALS = {"01-replay-by-seed", "02-control-the-random", "04-read-serial"}

directories = sorted(path for path in TUTORIALS.glob("[0-9][0-9]-*") if path.is_dir())
assert len(directories) == 29, f"expected 29 tutorials, found {len(directories)}"

for directory in directories:
    readme = directory / "README.md"
    assert readme.is_file(), f"{directory}: missing README.md"
    text = readme.read_text()
    assert re.search(r"from\s+(?:this\s+)?tutorial directory|from\s+this\s+directory", text), (
        f"{readme}: working directory is not explicit"
    )
    assert "../../../" not in text, f"{readme}: tutorial escapes its directory"
    assert "run-in-runtime.sh" not in text, f"{readme}: hides runtime steps"
    assert re.search(r"^## 1\. ", text, re.MULTILINE), f"{readme}: steps are not numbered"
    assert "```sh\n" in text, f"{readme}: no copyable shell commands"
    assert text.count("```") % 2 == 0, f"{readme}: unbalanced code fences"

    if directory.name not in HARNESS_TUTORIALS:
        assert "run.sh" not in text, f"{readme}: hides the procedure in run.sh"
        assert not (directory / "run.sh").exists(), f"{directory}: redundant run.sh"
    else:
        assert "Inspect" in text, f"{readme}: low-level harness is not reviewable"
        assert (directory / "run.sh").is_file(), f"{directory}: missing harness"

for directory in directories[13:]:
    text = (directory / "README.md").read_text()
    for heading in (
        "## Before you start",
        "## 1. Build",
        "## 2. Enter",
        "## 3. Prepare",
        "## 4. Run",
        "## 5. Inspect",
        "## 6. Clean up (optional)",
    ):
        assert heading in text, f"{directory}: missing {heading}"
    assert len(re.findall(r"```sh\n", text)) >= 6, f"{directory}: commands are not reviewable"
