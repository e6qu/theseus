#!/usr/bin/env python3
"""Seal a native runtime-validation directory with an exact file inventory."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re


ARCHITECTURES = ("amd64", "arm64")
SCENARIOS = (
    "container",
    "coverage",
    "schedule-search",
    "pthread-sync",
    "strict-execution",
)
COMMIT = re.compile(r"[0-9a-f]{40}")
DIGEST_IMAGE = re.compile(r".+@sha256:[0-9a-f]{64}")
REQUIRED = (
    "container/plan.json",
    "container/run/replay-plan.json",
    "container/run/result.json",
    "container/run/serial.log",
    "container/run/execution.json",
    "container/rerun/execution.json",
    "container/rerun/result.json",
    "container/rerun/serial.log",
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


def fail(message: str) -> None:
    raise ValueError(message)


def digest(path: Path) -> dict[str, object]:
    hasher = hashlib.sha256()
    size = 0
    with path.open("rb") as contents:
        while chunk := contents.read(1024 * 1024):
            hasher.update(chunk)
            size += len(chunk)
    return {"sha256": hasher.hexdigest(), "bytes": size}


def seal(args: argparse.Namespace) -> None:
    root = args.root.resolve()
    if not root.is_dir() or root.name != "validation":
        fail("--root must be the validation evidence directory")
    if COMMIT.fullmatch(args.source_commit) is None:
        fail("source commit must be a full lowercase Git SHA")
    if args.runtime_tag != f"{args.source_commit[:12]}-{args.architecture}":
        fail("runtime tag must be the source commit's short tag plus architecture")
    if DIGEST_IMAGE.fullmatch(args.runtime_image) is None:
        fail("runtime image must be pinned by digest")
    if args.kvm_api_version != 12:
        fail("KVM API version 12 is required")
    if not args.host_kernel:
        fail("host kernel release is required")
    missing = [name for name in REQUIRED if not (root / name).is_file()]
    if missing:
        fail("missing runtime validation evidence: " + ", ".join(missing))
    fixed = root / "fixed-plan"
    if fixed.exists():
        for name in ("certificate.json", "first/replay-plan.json", "replay/topology-result.json",
                     "first/checkpoint/starting-state/metadata.json", "first/checkpoint/starting-state/context.bin"):
            if not (fixed / name).is_file():
                fail("missing fixed-plan starting-state evidence: " + name)
    proof = root / "evidence.json"
    if proof.exists():
        fail("runtime validation proof already exists")

    files: dict[str, dict[str, object]] = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            fail(f"runtime validation contains a symbolic link: {path.relative_to(root)}")
        if path.is_file():
            files[path.relative_to(root).as_posix()] = digest(path)
    proof.write_text(
        json.dumps(
            {
                "format": "theseus-runtime-validation-v5",
                "architecture": args.architecture,
                "source_commit": args.source_commit,
                "runtime": {"image": args.runtime_image, "tag": args.runtime_tag},
                "host": {
                    "kernel_release": args.host_kernel,
                    "kvm_api_version": args.kvm_api_version,
                },
                "scenarios": list(SCENARIOS),
                "files": files,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--architecture", required=True, choices=ARCHITECTURES)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--runtime-image", required=True)
    parser.add_argument("--runtime-tag", required=True)
    parser.add_argument("--host-kernel", required=True)
    parser.add_argument("--kvm-api-version", required=True, type=int)
    args = parser.parse_args()
    try:
        seal(args)
    except ValueError as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
