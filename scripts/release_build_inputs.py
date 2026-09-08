#!/usr/bin/env python3
"""Write the immutable inputs and published digests for one runtime release."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[1]
DOCKERFILE = ROOT / "Dockerfile"
SHA256 = re.compile(r"sha256:[0-9a-f]{64}$")
COMMIT = re.compile(r"[0-9a-f]{40}$")


def docker_inputs(dockerfile: Path) -> dict[str, object]:
    contents = dockerfile.read_text()
    frontend = re.search(r"^# syntax=(\S+)$", contents, re.MULTILINE)
    bases = re.findall(r"^FROM (\S+) AS \S+$", contents, re.MULTILINE)
    revision = re.search(r"^ARG THESEUS_KERNEL_REVISION=([0-9a-f]{40})$", contents, re.MULTILINE)
    snapshots = sorted(set(re.findall(r"https?://snapshot\.debian\.org/[A-Za-z0-9._/-]+", contents)))
    if frontend is None or len(bases) != 2 or revision is None or len(snapshots) != 4:
        raise ValueError("Dockerfile does not declare the complete immutable runtime input set")
    if not all("@sha256:" in base for base in bases):
        raise ValueError("runtime base images must use immutable digests")
    return {
        "dockerfile_frontend": frontend.group(1),
        "base_images": {"build": bases[0], "runtime": bases[1]},
        "apt_snapshots": snapshots,
        "kernel_revision": revision.group(1),
    }


def required_sha256(value: str) -> str:
    if not SHA256.fullmatch(value):
        raise argparse.ArgumentTypeError(f"not a SHA-256 digest: {value}")
    return value


def required_commit(value: str) -> str:
    if not COMMIT.fullmatch(value):
        raise argparse.ArgumentTypeError(f"not a Git commit: {value}")
    return value


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--source-repository", required=True)
    parser.add_argument("--source-commit", required=True, type=required_commit)
    parser.add_argument("--source-date-epoch", required=True, type=int)
    parser.add_argument("--runtime-image", required=True)
    parser.add_argument("--runtime-tag", required=True)
    parser.add_argument("--runtime-manifest-digest", required=True, type=required_sha256)
    parser.add_argument("--runtime-amd64-digest", required=True, type=required_sha256)
    parser.add_argument("--runtime-arm64-digest", required=True, type=required_sha256)
    args = parser.parse_args()
    if args.source_date_epoch < 0:
        parser.error("--source-date-epoch must be non-negative")

    record = {
        "schema": 1,
        "source": {
            "repository": args.source_repository,
            "commit": args.source_commit,
            "date_epoch": args.source_date_epoch,
        },
        "runtime": {
            "image": args.runtime_image,
            "tag": args.runtime_tag,
            "manifest_digest": args.runtime_manifest_digest,
            "platform_digests": {
                "amd64": args.runtime_amd64_digest,
                "arm64": args.runtime_arm64_digest,
            },
        },
        "build": docker_inputs(DOCKERFILE),
    }
    args.output.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
