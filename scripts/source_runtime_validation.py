#!/usr/bin/env python3
"""Describe source-native validation without claiming published runtime proof."""

import argparse
import json
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
    missing = [name for name in REQUIRED if not (args.root / name).is_file()]
    if missing:
        parser.error("missing source validation files: " + ", ".join(missing))
    runtime = {}
    for name in ("theseus", "theseus-topology", "theseus-image", "firecracker", "vmlinux"):
        runtime[name] = digest(args.runtime / name)
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
