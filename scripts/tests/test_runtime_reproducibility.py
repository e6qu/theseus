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
    assert "ARG THESEUS_SOURCE_COMMIT=unknown" in DOCKERFILE
    assert 'orchestrator/pivot/build.sh --target "$TARGETARCH"' in DOCKERFILE
    assert "theseus-image pivot > /out/embedded-pivot.json" in DOCKERFILE
    assert "cp orchestrator/pivot.bin /out/pivot" in DOCKERFILE
    assert "instrumentation/c/theseus-coverage-cc" in DOCKERFILE
    assert "instrumentation/c/theseus_coverage.c" in DOCKERFILE
    assert "instrumentation/c/theseus-schedule-cc" in DOCKERFILE
    assert "instrumentation/c/theseus_schedule.c" in DOCKERFILE
    assert "instrumentation/llvm/theseus-coverage-clang" in DOCKERFILE
    assert "instrumentation/llvm/theseus-coverage-rustc" in DOCKERFILE
    assert "instrumentation/llvm/theseus-coverage-inspect" in DOCKERFILE
    assert "instrumentation/llvm/theseus_coverage.c" in DOCKERFILE
    assert "libclang-rt-dev" in DOCKERFILE
    assert "cargo clang" in DOCKERFILE
    assert "libseccomp2 rustc" in DOCKERFILE

    assert "SOURCE_DATE_EPOCH: ${{ steps.source-date.outputs.value }}" in RELEASE
    assert "THESEUS_SOURCE_COMMIT=${{ github.sha }}" in RELEASE
    assert "/opt/theseus/pivot.json" in RELEASE
    assert "docker image inspect" in RELEASE
    assert 'docker run --rm --platform "linux/${{ matrix.arch }}"' in RELEASE
    assert 'runtime="${IMAGE}:${TAG}-${{ matrix.arch }}"' in RELEASE
    assert RELEASE.count("pivot_metadata=$(docker run") == 2
    assert RELEASE.count("pivot_manifest=$(docker run") == 2
    assert RELEASE.count("pivot_file_sha256=$(docker run") == 2
    assert RELEASE.count('jq -r .architecture <<< "$pivot_metadata"') == 2
    assert RELEASE.count('jq -r .source_commit <<< "$pivot_manifest"') == 2
    assert RELEASE.count("test -x /opt/theseus/instrumentation/c/theseus-coverage-cc") == 2
    assert "test -x instrumentation/c/theseus-coverage-cc" in RELEASE
    assert RELEASE.count("test -x /opt/theseus/instrumentation/c/theseus-schedule-cc") == 2
    assert "test -x instrumentation/c/theseus-schedule-cc" in RELEASE
    assert RELEASE.count("test -x /opt/theseus/instrumentation/llvm/theseus-coverage-clang") == 2
    assert RELEASE.count("test -x /opt/theseus/instrumentation/llvm/theseus-coverage-rustc") == 2
    assert "test -x instrumentation/llvm/theseus-coverage-clang" in RELEASE
    assert RELEASE.count("--process smoke --module c -o") == 2
    assert RELEASE.count("--process smoke --module rust") == 2
    assert RELEASE.count("theseus coverage cargo") == 2
    assert RELEASE.count("--process smoke --module cargo") == 2
    assert "gh release download" in VERIFY
    assert "gh attestation verify" in VERIFY
    assert "ref: ${{ steps.inputs.outputs.commit }}" in VERIFY
    assert "--no-cache" in VERIFY
    assert "--platform \"linux/${{ matrix.arch }}\"" in VERIFY
    assert 'THESEUS_SOURCE_COMMIT=${{ steps.inputs.outputs.commit }}' in VERIFY
    assert "type=oci,dest=$work/${build}.oci.tar,rewrite-timestamp=true" in VERIFY
    assert "diff -ru \"$work/first\" \"$work/second\"" in VERIFY
    assert "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6" in VERIFY
    assert "ubuntu-24.04-arm" in VERIFY


if __name__ == "__main__":
    main()
