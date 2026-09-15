#!/usr/bin/env python3
"""Exercise native runtime-validation evidence sealing."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/runtime_validation_evidence.py"
COMMIT = "0123456789abcdef0123456789abcdef01234567"
REQUIRED = (
    "container/plan.json",
    "container/run/replay-plan.json",
    "container/run/result.json",
    "container/run/serial.log",
    "container/replay.log",
    "container/source/Dockerfile",
    "container/source/theseus.toml",
    "coverage/plan.json",
    "coverage/campaign/campaign-result.json",
    "coverage/campaign/replay-plan.json",
    "coverage/report/report.md",
    "coverage/rerun/campaign-result.json",
    "coverage/comparison.json",
    "coverage/evaluation.json",
    "coverage/evaluation/theseus-evaluation.toml",
    "coverage/evaluation/theseus-evaluation.lock",
    "coverage/source/compose.yaml",
    "coverage/source/service/main.c",
    "coverage/source/service/theseus.toml",
    "schedule-search/plan.json",
    "schedule-search/campaign/campaign-result.json",
    "schedule-search/campaign/replay-plan.json",
    "schedule-search/report/report.md",
    "schedule-search/minimized/minimization.json",
    "schedule-search/minimized/replay-plan.json",
    "schedule-search/rerun/campaign-result.json",
    "schedule-search/source/compose.yaml",
    "schedule-search/source/service/main.c",
    "schedule-search/source/service/theseus.toml",
    "pthread-sync/plan.json",
    "pthread-sync/campaign/campaign-result.json",
    "pthread-sync/campaign/replay-plan.json",
    "pthread-sync/report/report.md",
    "pthread-sync/rerun/campaign-result.json",
    "pthread-sync/source/compose.yaml",
    "pthread-sync/source/service/main.c",
    "pthread-sync/source/service/theseus.toml",
    "strict-execution/plan.json",
    "strict-execution/campaign/campaign-result.json",
    "strict-execution/campaign/replay-plan.json",
    "strict-execution/report/report.md",
    "strict-execution/rerun/campaign-result.json",
    "strict-execution/comparison.json",
    "strict-execution/source/.dockerignore",
    "strict-execution/source/Dockerfile",
    "strict-execution/source/compose.yaml",
    "strict-execution/source/api/theseus.toml",
)


def run(root: Path, success: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--root",
            str(root),
            "--architecture",
            "amd64",
            "--source-commit",
            COMMIT,
            "--runtime-image",
            "ghcr.io/e6qu/theseus@sha256:" + "a" * 64,
            "--runtime-tag",
            COMMIT[:12] + "-amd64",
            "--host-kernel",
            "6.8.0",
            "--kvm-api-version",
            "12",
        ],
        text=True,
        capture_output=True,
    )
    if success and result.returncode != 0:
        raise AssertionError(result.stderr)
    if not success and result.returncode == 0:
        raise AssertionError("command unexpectedly succeeded")
    return result


def main() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        validation = Path(temporary) / "validation"
        for name in REQUIRED:
            path = validation / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(name + "\n")
        run(validation)
        evidence = json.loads((validation / "evidence.json").read_text())
        assert evidence["format"] == "theseus-runtime-validation-v2"
        assert evidence["architecture"] == "amd64"
        assert evidence["source_commit"] == COMMIT
        assert evidence["scenarios"] == [
            "container",
            "coverage",
            "schedule-search",
            "pthread-sync",
            "strict-execution",
        ]
        assert set(evidence["files"]) == set(REQUIRED)
        assert all(len(item["sha256"]) == 64 for item in evidence["files"].values())
        duplicate = run(validation, success=False)
        assert "proof already exists" in duplicate.stderr

    with tempfile.TemporaryDirectory() as temporary:
        validation = Path(temporary) / "validation"
        validation.mkdir()
        missing = run(validation, success=False)
        assert "missing runtime validation evidence" in missing.stderr


if __name__ == "__main__":
    main()
