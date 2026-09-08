#!/usr/bin/env python3
"""Create a deterministic gzip-compressed tar archive from one directory."""

from __future__ import annotations

import argparse
import gzip
import os
from pathlib import Path
import tarfile


def add_tree(archive: tarfile.TarFile, source: Path, mtime: int) -> None:
    root = source.name
    for directory, directories, files in os.walk(source, followlinks=False):
        directories.sort()
        files.sort()
        path = Path(directory)
        entries = [path / name for name in directories + files]
        if path == source:
            entries.insert(0, path)
        for entry in entries:
            name = Path(root) / entry.relative_to(source)
            info = archive.gettarinfo(str(entry), arcname=str(name))
            info.uid = 0
            info.gid = 0
            info.uname = ""
            info.gname = ""
            info.mtime = mtime
            if info.isreg():
                with entry.open("rb") as contents:
                    archive.addfile(info, contents)
            else:
                archive.addfile(info)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mtime", required=True, type=int, help="Unix timestamp for every entry")
    parser.add_argument("--output", required=True, type=Path, help="archive to write")
    parser.add_argument("source", type=Path, help="directory to archive")
    args = parser.parse_args()

    source = args.source.resolve()
    if not source.is_dir():
        parser.error(f"source must be a directory: {args.source}")
    if args.mtime < 0:
        parser.error("--mtime must be a non-negative Unix timestamp")

    with args.output.open("wb") as raw:
        with gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=args.mtime) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as archive:
                add_tree(archive, source, args.mtime)


if __name__ == "__main__":
    main()
