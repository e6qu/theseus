#!/usr/bin/env python3
"""Tabulate the weighting experiment's yield across recent CI runs.

The seed-default decision reads accumulated recordings, not one run. This
script fetches the most recent `weighting-experiment` artifacts from the
repository's Actions history, parses each cross-workload summary's yield
table (witnesses per run and first witness position, seeded minus
unseeded), and prints one combined trend table oldest-first.

Requires the `gh` CLI authenticated against the repository. Usage:

    weighting_history.py [--runs N] [--repo OWNER/NAME]

`--runs` bounds how many of the newest artifacts to read (default 10).
"""

import argparse
import io
import json
import subprocess
import sys
import zipfile
from pathlib import Path


def gh(*args: str) -> bytes:
    return subprocess.run(
        ["gh", "api", *args],
        check=True,
        capture_output=True,
    ).stdout


def newest_artifacts(repo: str, limit: int):
    listing = json.loads(
        gh(f"repos/{repo}/actions/artifacts?per_page=100").decode()
    )
    artifacts = [
        a
        for a in listing.get("artifacts", [])
        if a.get("name") == "weighting-experiment"
        and not a.get("expired", False)
    ]
    artifacts.sort(key=lambda a: a["created_at"], reverse=True)
    return artifacts[:limit]


def yield_rows(archive: bytes):
    """Parse one artifact's yield table: (workload, budget, row) tuples."""
    rows = []
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        names = [n for n in bundle.namelist() if n.endswith("summary.md")]
        if not names:
            return rows
        text = bundle.read(names[0]).decode()
    section = text.split("## Yield comparison", 1)[-1]
    for line in section.splitlines():
        if not line.startswith("| ") or line.startswith("| ---"):
            continue
        cells = [cell.strip() for cell in line.split("|")]
        if len(cells) < 7 or cells[1] in ("workload", ""):
            continue
        rows.append((cells[1], cells[2], cells[3], cells[4], cells[5], cells[6]))
    return rows


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument(
        "--repo",
        default=None,
        help="OWNER/NAME; defaults to the checkout's origin",
    )
    args = parser.parse_args()
    repo = args.repo
    if repo is None:
        origin = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            check=True,
            capture_output=True,
        ).stdout.decode().strip()
        # Both remote forms carry OWNER/NAME at the tail:
        # git@github.com:OWNER/NAME.git and https://github.com/OWNER/NAME.git
        tail = origin.rstrip("/").removesuffix(".git")
        repo = tail.split("/")[-2] + "/" + tail.split("/")[-1]

    artifacts = newest_artifacts(repo, args.runs)
    if not artifacts:
        print("no weighting-experiment artifacts found")
        return

    print(
        "| recorded | workload | budget | unseeded/run | seeded/run "
        "| delta | first witness (unseeded/seeded) |"
    )
    print("| --- | --- | --- | --- | --- | --- | --- |")
    for artifact in reversed(artifacts):
        recorded = artifact["created_at"][:16]
        archive = gh(
            f"repos/{repo}/actions/artifacts/{artifact['id']}/zip"
        )
        for workload, budget, unseeded, seeded, delta, positions in yield_rows(
            archive
        ):
            print(
                f"| {recorded} | {workload} | {budget} | {unseeded} "
                f"| {seeded} | {delta} | {positions} |"
            )


if __name__ == "__main__":
    main()
