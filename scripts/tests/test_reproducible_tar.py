#!/usr/bin/env python3
"""Exercise the portable deterministic archive writer."""

from __future__ import annotations

import hashlib
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "reproducible_tar.py"


def archive(source: Path, output: Path) -> bytes:
    subprocess.run(
        [sys.executable, str(SCRIPT), "--mtime", "1700000000", "--output", str(output), str(source)],
        check=True,
    )
    return output.read_bytes()


def main() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = root / "bundle"
        nested = source / "nested"
        nested.mkdir(parents=True)
        executable = source / "theseus"
        executable.write_text("#!/bin/sh\necho theseus\n")
        executable.chmod(0o755)
        (nested / "configuration").write_text("fixed input\n")

        first = archive(source, root / "first.tar.gz")
        executable.touch()
        (nested / "configuration").touch()
        second = archive(source, root / "second.tar.gz")
        assert hashlib.sha256(first).digest() == hashlib.sha256(second).digest()

        with tarfile.open(root / "first.tar.gz", "r:gz") as contents:
            members = contents.getmembers()
        assert [member.name for member in members] == [
            "bundle",
            "bundle/nested",
            "bundle/theseus",
            "bundle/nested/configuration",
        ]
        assert all(member.mtime == 1_700_000_000 for member in members)
        assert all((member.uid, member.gid, member.uname, member.gname) == (0, 0, "", "") for member in members)
        assert next(member for member in members if member.name == "bundle/theseus").mode == 0o755


if __name__ == "__main__":
    main()
