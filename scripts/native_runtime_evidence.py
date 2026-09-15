#!/usr/bin/env python3
"""Seal one native counterexample or index released native evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re


ARCHITECTURES = ("amd64", "arm64")
COMMIT = re.compile(r"[0-9a-f]{40}")
PROPERTY = "distributed_lost_update_is_unreachable"
REQUIRED_FAULTS = [
    "backplane:partition@setup",
    "backplane:heal@probe_partition",
]


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


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def require_commit(value: str) -> None:
    if COMMIT.fullmatch(value) is None:
        fail("source commit must be a full lowercase Git SHA")


def seal(args: argparse.Namespace) -> None:
    root = args.root.resolve()
    if not root.is_dir() or root.name != "minimized":
        fail("--root must be the minimized counterexample directory")
    require_commit(args.source_commit)
    if args.runtime_tag != f"{args.source_commit[:12]}-{args.architecture}":
        fail("runtime tag must be the source commit's 12-character tag plus architecture")
    if re.fullmatch(r".+@sha256:[0-9a-f]{64}", args.runtime_image) is None:
        fail("runtime image must be pinned by digest")
    if args.kvm_api_version != 12:
        fail("KVM API version 12 is required")
    if not args.host_kernel:
        fail("host kernel release is required")
    required = [
        root / "evidence/runtime-certificate.json",
        root / "evidence/campaign-result.json",
        root / "evidence/replay/topology-result.json",
        root / "evidence/replay/services/counter/serial.log",
        root / "evidence/replay/services/writer-a/serial.log",
        root / "evidence/replay/services/counter/result.json",
        root / "evidence/replay/services/writer-a/result.json",
        root / "evidence/replay/services/writer-b/result.json",
        root / "minimization.json",
    ]
    missing = [str(path.relative_to(root)) for path in required if not path.is_file()]
    if missing:
        fail("missing counterexample evidence: " + ", ".join(missing))

    proof_path = root / "evidence/proof.json"
    if proof_path.exists():
        fail("counterexample proof already exists")
    files: dict[str, dict[str, object]] = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            fail(f"counterexample contains a symbolic link: {path.relative_to(root)}")
        if path.is_file():
            files[path.relative_to(root).as_posix()] = digest(path)
    proof = {
        "format": "theseus-counterexample-proof-v2",
        "architecture": args.architecture,
        "source_commit": args.source_commit,
        "runtime": {"image": args.runtime_image, "tag": args.runtime_tag},
        "host": {
            "kernel_release": args.host_kernel,
            "kvm_api_version": args.kvm_api_version,
        },
        "property": PROPERTY,
        "required_faults": REQUIRED_FAULTS,
        "files": files,
    }
    proof_path.parent.mkdir(parents=True, exist_ok=True)
    write_json(proof_path, proof)


def asset(directory: Path, name: str) -> dict[str, object]:
    path = directory / name
    if not path.is_file():
        fail(f"missing native evidence asset: {name}")
    return {"file": name, **digest(path)}


def index(args: argparse.Namespace) -> None:
    directory = args.directory.resolve()
    require_commit(args.source_commit)
    if args.tag != args.source_commit[:12]:
        fail("release tag must match the source commit")
    architectures: dict[str, object] = {}
    for architecture in args.architectures:
        certificate = f"theseus-{args.tag}-runtime-certificate-{architecture}.json"
        counterexample = f"theseus-{args.tag}-multiservice-counterexample-{architecture}.tar.gz"
        validation = f"theseus-{args.tag}-runtime-validation-{architecture}.tar.gz"
        architectures[architecture] = {
            "certificate": asset(directory, certificate),
            "counterexample": asset(directory, counterexample),
            "validation": asset(directory, validation),
        }
    output = args.output.resolve()
    if output.parent != directory:
        fail("native evidence index must be written beside its assets")
    write_json(
        output,
        {
            "format": "theseus-native-evidence-index-v2",
            "source_commit": args.source_commit,
            "runtime_tag": args.tag,
            "architectures": architectures,
        },
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    seal_parser = commands.add_parser("seal", help="write proof.json and its complete file inventory")
    seal_parser.add_argument("--root", required=True, type=Path)
    seal_parser.add_argument("--architecture", required=True, choices=ARCHITECTURES)
    seal_parser.add_argument("--source-commit", required=True)
    seal_parser.add_argument("--runtime-image", required=True)
    seal_parser.add_argument("--runtime-tag", required=True)
    seal_parser.add_argument("--host-kernel", required=True)
    seal_parser.add_argument("--kvm-api-version", required=True, type=int)
    seal_parser.set_defaults(action=seal)
    index_parser = commands.add_parser("index", help="index one or more architecture evidence sets")
    index_parser.add_argument("--directory", required=True, type=Path)
    index_parser.add_argument("--tag", required=True)
    index_parser.add_argument("--source-commit", required=True)
    index_parser.add_argument(
        "--architectures",
        nargs="+",
        choices=ARCHITECTURES,
        default=list(ARCHITECTURES),
    )
    index_parser.add_argument("--output", required=True, type=Path)
    index_parser.set_defaults(action=index)
    args = parser.parse_args()
    try:
        args.action(args)
    except ValueError as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
