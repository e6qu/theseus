#!/usr/bin/env python3
"""Source qualification must not impersonate published native evidence."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from runtime_validation_evidence import REQUIRED


def main():
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        validation = directory / "validation"
        runtime = directory / "runtime"
        runtime.mkdir()
        for name in REQUIRED:
            path = validation / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("retained input\n")
        for name in ("theseus", "theseus-topology", "theseus-image", "firecracker", "vmlinux"):
            (runtime / name).write_text("source " + name)
        arguments = [sys.executable, str(ROOT / "scripts/source_runtime_validation.py"),
            "--root", str(validation), "--runtime", str(runtime), "--architecture", "amd64",
            "--source-commit", "a" * 40, "--compiler-image", "ghcr.io/e6qu/theseus@sha256:" + "b" * 64,
            "--kvm-api-version", "12"]
        result = subprocess.run(arguments, capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        descriptor = json.loads((validation / "source-validation.json").read_text())
        assert descriptor["format"] == "theseus-source-runtime-validation-v1"
        assert descriptor["release_qualification"] is False
        assert len(descriptor["runtime_artifacts"]) == 5
        assert not (validation / "evidence.json").exists()
        for flag, value in [("--source-commit", "a" * 12), ("--compiler-image", "ghcr.io/e6qu/theseus:old"),
            ("--kvm-api-version", "11")]:
            invalid = arguments.copy()
            invalid[invalid.index(flag) + 1] = value
            assert subprocess.run(invalid, capture_output=True).returncode != 0
        (runtime / "firecracker").write_text("")
        assert subprocess.run(arguments, capture_output=True).returncode != 0
        (runtime / "firecracker").unlink()
        (runtime / "firecracker").symlink_to("theseus")
        assert subprocess.run(arguments, capture_output=True).returncode != 0


if __name__ == "__main__":
    main()
