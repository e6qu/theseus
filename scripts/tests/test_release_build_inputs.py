#!/usr/bin/env python3
"""Exercise the signed release build-input inventory generator."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "release_build_inputs.py"
COMMIT = "a" * 40
DIGEST = "sha256:" + "b" * 64


def generate(output: Path) -> bytes:
    subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--output",
            str(output),
            "--source-repository",
            "https://github.com/e6qu/theseus",
            "--source-commit",
            COMMIT,
            "--source-date-epoch",
            "1700000000",
            "--runtime-image",
            "ghcr.io/e6qu/theseus",
            "--runtime-tag",
            "0123456789ab",
            "--runtime-manifest-digest",
            DIGEST,
            "--runtime-amd64-digest",
            DIGEST,
            "--runtime-arm64-digest",
            DIGEST,
        ],
        check=True,
    )
    return output.read_bytes()


def main() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        first = generate(directory / "first.json")
        second = generate(directory / "second.json")
        assert first == second
        record = json.loads(first)
    assert record["schema"] == 1
    assert record["source"] == {
        "repository": "https://github.com/e6qu/theseus",
        "commit": COMMIT,
        "date_epoch": 1_700_000_000,
    }
    assert record["runtime"]["platform_digests"] == {"amd64": DIGEST, "arm64": DIGEST}
    assert record["build"]["kernel_revision"] == "8a40ca92bfa9b706b76287942c89b13884928cb0"
    assert len(record["build"]["apt_snapshots"]) == 4


if __name__ == "__main__":
    main()
