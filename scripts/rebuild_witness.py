#!/usr/bin/env python3
"""Write an attested statement for one independently rebuilt runtime image."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re


COMMIT = re.compile(r"[0-9a-f]{40}$")
SHA256 = re.compile(r"sha256:[0-9a-f]{64}$")
RAW_SHA256 = re.compile(r"[0-9a-f]{64}$")


def required(pattern: re.Pattern[str], label: str):
    def validate(value: str) -> str:
        if not pattern.fullmatch(value):
            raise argparse.ArgumentTypeError(f"not a {label}: {value}")
        return value

    return validate


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--source-commit", required=True, type=required(COMMIT, "Git commit"))
    parser.add_argument("--source-date-epoch", required=True, type=int)
    parser.add_argument("--inputs-sha256", required=True, type=required(RAW_SHA256, "SHA-256"))
    parser.add_argument("--architecture", required=True, choices=("amd64", "arm64"))
    parser.add_argument("--expected-digest", required=True, type=required(SHA256, "OCI digest"))
    parser.add_argument("--actual-digest", required=True, type=required(SHA256, "OCI digest"))
    args = parser.parse_args()
    if args.source_date_epoch < 0:
        parser.error("--source-date-epoch must be non-negative")
    if args.expected_digest != args.actual_digest:
        parser.error("rebuilt OCI digest does not match the signed release input")

    witness = {
        "schema": 1,
        "release": {
            "repository": args.repository,
            "tag": args.tag,
            "build_inputs_sha256": args.inputs_sha256,
        },
        "source": {"commit": args.source_commit, "date_epoch": args.source_date_epoch},
        "runtime": {
            "architecture": args.architecture,
            "expected_digest": args.expected_digest,
            "actual_digest": args.actual_digest,
        },
    }
    args.output.write_text(json.dumps(witness, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
