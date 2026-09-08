#!/usr/bin/env python3
"""Keep the immutable runtime inputs and proof workflow wired together."""

from __future__ import annotations

from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[2]
DOCKERFILE = (ROOT / "Dockerfile").read_text()
RELEASE = (ROOT / ".github/workflows/release.yml").read_text()
VERIFY = (ROOT / ".github/workflows/verify-runtime-reproducibility.yml").read_text()


def main() -> None:
    assert re.search(r"^# syntax=docker/dockerfile:1@sha256:[0-9a-f]{64}$", DOCKERFILE, re.MULTILINE)
    assert len(re.findall(r"^FROM .+@sha256:[0-9a-f]{64} AS ", DOCKERFILE, re.MULTILINE)) == 2
    assert "snapshot.debian.org/archive/debian/20260713T000000Z" in DOCKERFILE
    assert "snapshot.debian.org/archive/debian/20260824T000000Z" in DOCKERFILE
    assert "snapshot.debian.org/archive/debian-security/" in DOCKERFILE
    assert 'Acquire::Check-Valid-Until "false";' in DOCKERFILE
    assert "THESEUS_KERNEL_REVISION=" in DOCKERFILE

    assert "SOURCE_DATE_EPOCH: ${{ steps.source-date.outputs.value }}" in RELEASE
    assert "--no-cache" in VERIFY
    assert "type=oci,dest=${build}.oci.tar,rewrite-timestamp=true" in VERIFY
    assert "diff -ru first second" in VERIFY
    assert "ubuntu-24.04-arm" in VERIFY


if __name__ == "__main__":
    main()
