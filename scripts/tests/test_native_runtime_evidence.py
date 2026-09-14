#!/usr/bin/env python3
"""Exercise deterministic native evidence sealing and pair indexing."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/native_runtime_evidence.py"
COMMIT = "0123456789abcdef0123456789abcdef01234567"
TAG = COMMIT[:12]
DIGEST = "sha256:" + "a" * 64


def run(*arguments: str, success: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [sys.executable, str(SCRIPT), *arguments],
        text=True,
        capture_output=True,
    )
    if success and result.returncode != 0:
        raise AssertionError(result.stderr)
    if not success and result.returncode == 0:
        raise AssertionError("command unexpectedly succeeded")
    return result


def write_counterexample(root: Path) -> None:
    paths = [
        "evidence/runtime-certificate.json",
        "evidence/campaign-result.json",
        "evidence/replay/topology-result.json",
        "evidence/replay/services/counter/serial.log",
        "evidence/replay/services/writer-a/serial.log",
        "evidence/replay/services/counter/result.json",
        "evidence/replay/services/writer-a/result.json",
        "evidence/replay/services/writer-b/result.json",
        "minimization.json",
        "replay-plan.json",
    ]
    for name in paths:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"{name}\n")


def main() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        minimized = directory / "minimized"
        write_counterexample(minimized)
        run(
            "seal",
            "--root",
            str(minimized),
            "--architecture",
            "amd64",
            "--source-commit",
            COMMIT,
            "--runtime-image",
            f"ghcr.io/e6qu/theseus@{DIGEST}",
            "--runtime-tag",
            f"{TAG}-amd64",
            "--host-kernel",
            "6.8.0",
            "--kvm-api-version",
            "12",
        )
        proof = json.loads((minimized / "evidence/proof.json").read_text())
        assert proof["format"] == "theseus-counterexample-proof-v2"
        assert proof["architecture"] == "amd64"
        assert proof["runtime"]["tag"] == f"{TAG}-amd64"
        assert "replay-plan.json" in proof["files"]
        recorded = proof["files"]["replay-plan.json"]
        assert recorded == {
            "bytes": len("replay-plan.json\n"),
            "sha256": hashlib.sha256(b"replay-plan.json\n").hexdigest(),
        }
        duplicate = run(
            "seal",
            "--root",
            str(minimized),
            "--architecture",
            "amd64",
            "--source-commit",
            COMMIT,
            "--runtime-image",
            f"ghcr.io/e6qu/theseus@{DIGEST}",
            "--runtime-tag",
            f"{TAG}-amd64",
            "--host-kernel",
            "6.8.0",
            "--kvm-api-version",
            "12",
            success=False,
        )
        assert "proof already exists" in duplicate.stderr

        for architecture in ("amd64", "arm64"):
            for suffix in (
                f"runtime-certificate-{architecture}.json",
                f"multiservice-counterexample-{architecture}.tar.gz",
            ):
                (directory / f"theseus-{TAG}-{suffix}").write_text(suffix)
        index_path = directory / f"theseus-{TAG}-native-evidence.json"
        run(
            "index",
            "--directory",
            str(directory),
            "--tag",
            TAG,
            "--source-commit",
            COMMIT,
            "--output",
            str(index_path),
        )
        index = json.loads(index_path.read_text())
        assert list(index["architectures"]) == ["amd64", "arm64"]
        assert index["source_commit"] == COMMIT
        for evidence in index["architectures"].values():
            assert len(evidence["certificate"]["sha256"]) == 64
            assert len(evidence["counterexample"]["sha256"]) == 64


if __name__ == "__main__":
    main()
