#!/usr/bin/env python3
"""Exercise the independently rebuilt runtime witness writer."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "rebuild_witness.py"
COMMIT = "a" * 40
DIGEST = "sha256:" + "b" * 64
INPUTS_SHA256 = "c" * 64


def command(output: Path, actual: str) -> list[str]:
    return [
        sys.executable,
        str(SCRIPT),
        "--output",
        str(output),
        "--repository",
        "e6qu/theseus",
        "--tag",
        "0123456789ab",
        "--source-commit",
        COMMIT,
        "--source-date-epoch",
        "1700000000",
        "--inputs-sha256",
        INPUTS_SHA256,
        "--architecture",
        "arm64",
        "--expected-digest",
        DIGEST,
        "--actual-digest",
        actual,
    ]


def main() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        output = Path(temporary) / "witness.json"
        subprocess.run(command(output, DIGEST), check=True)
        witness = json.loads(output.read_text())
        rejected = subprocess.run(command(output, "sha256:" + "d" * 64), capture_output=True)
    assert rejected.returncode != 0
    assert witness["release"]["build_inputs_sha256"] == INPUTS_SHA256
    assert witness["runtime"] == {
        "architecture": "arm64",
        "expected_digest": DIGEST,
        "actual_digest": DIGEST,
    }


if __name__ == "__main__":
    main()
