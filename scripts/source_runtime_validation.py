#!/usr/bin/env python3
"""Describe source-native validation without claiming published runtime proof."""

import argparse
import json
import re
from pathlib import Path

from runtime_validation_evidence import REQUIRED, SCENARIOS, digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--runtime", required=True, type=Path)
    parser.add_argument("--architecture", required=True, choices=("amd64", "arm64"))
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--compiler-image", required=True)
    parser.add_argument("--kvm-api-version", required=True, type=int)
    args = parser.parse_args()
    if args.kvm_api_version != 12:
        parser.error("native KVM API version 12 is required")
    if re.fullmatch(r"[0-9a-f]{40}", args.source_commit) is None:
        parser.error("source commit must be a full lowercase Git SHA")
    if re.fullmatch(r"ghcr\.io/e6qu/theseus@sha256:[0-9a-f]{64}", args.compiler_image) is None:
        parser.error("compiler dependency must be pinned by digest")
    missing = [name for name in REQUIRED if not (args.root / name).is_file()]
    if missing:
        parser.error("missing source validation files: " + ", ".join(missing))
    runtime = {}
    for name in ("theseus", "theseus-topology", "theseus-image", "firecracker", "vmlinux"):
        if (args.runtime / name).is_symlink() or not (args.runtime / name).is_file():
            parser.error("source runtime member must be a regular file: " + name)
        runtime[name] = digest(args.runtime / name)
        if runtime[name]["bytes"] == 0:
            parser.error("source runtime member is empty: " + name)
    descriptor = {
        "format": "theseus-source-runtime-validation-v1",
        "architecture": args.architecture,
        "source_commit": args.source_commit,
        "compiler_image": args.compiler_image,
        "runtime_artifacts": runtime,
        "kvm_api_version": args.kvm_api_version,
        "scenarios": list(SCENARIOS),
        "release_qualification": False,
    }
    (args.root / "source-validation.json").write_text(json.dumps(descriptor, indent=2) + "\n")


if __name__ == "__main__":
    main()
