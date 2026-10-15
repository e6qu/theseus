#!/usr/bin/env python3
"""Tabulate the weighting experiment's yield across recent CI runs.

The seed-default decision reads accumulated recordings, not one run. This
script fetches the most recent `weighting-experiment` artifacts from the
repository's Actions history, parses each cross-workload summary's yield
table (witnesses per run and first witness position, seeded minus
unseeded), and prints the trend oldest-first - as a markdown table, or
with `--json` as one JSON document for machine reads.

Requires the `gh` CLI authenticated against the repository. Usage:

    weighting_history.py [--runs N] [--repo OWNER/NAME] [--json]
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
    """Parse one artifact's yield table: (workload, budget, unseeded/run,
    seeded/run, delta, first-witness positions) per row."""
    rows = []
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        summary = next(
            (n for n in bundle.namelist() if n.endswith("summary.md")), None
        )
        if summary is None:
            return rows
        text = bundle.read(summary).decode()
    section = text.split("## Yield comparison", 1)[-1]
    for line in section.splitlines():
        if not line.startswith("| ") or line.startswith("| ---"):
            continue
        cells = [cell.strip() for cell in line.split("|")]
        if len(cells) < 7 or cells[1] in ("workload", ""):
            continue
        rows.append(
            {
                "workload": cells[1],
                "budget": cells[2],
                "unseeded_per_run": cells[3],
                "seeded_per_run": cells[4],
                "delta": cells[5],
                "first_witness": cells[6],
            }
        )
    return rows


def origin_repo() -> str:
    origin = subprocess.run(
        ["git", "remote", "get-url", "origin"],
        check=True,
        capture_output=True,
    )
    tail = origin.stdout.decode().strip()
    # Both remote forms carry OWNER/NAME at the tail:
    # git@github.com:OWNER/NAME.git and https://github.com/OWNER/NAME.git
    parts = tail.removesuffix(".git").rstrip("/").split("/")
    return f"{parts[-2]}/{parts[-1]}"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument(
        "--repo",
        default=None,
        help="OWNER/NAME; defaults to the checkout's origin",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit one JSON document instead of the markdown table",
    )
    args = parser.parse_args()
    repo = args.repo or origin_repo()

    artifacts = newest_artifacts(repo, args.runs)
    if not artifacts:
        print("no weighting-experiment artifacts found")
        return

    collected = []
    for artifact in reversed(artifacts):
        archive = gh(f"repos/{repo}/actions/artifacts/{artifact['id']}/zip")
        recorded = artifact["created_at"][:16]
        for row in yield_rows(archive):
            collected.append({"recorded": recorded, **row})

    if args.json:
        print(json.dumps(collected, indent=2))
        return

    print(
        "| recorded | workload | budget | unseeded/run | seeded/run "
        "| delta | first witness (unseeded/seeded) |"
    )
    print("| " + " | ".join(["---"] * 7) + " |")
    for row in collected:
        print(
            f"| {row['recorded']} | {row['workload']} | {row['budget']} "
            f"| {row['unseeded_per_run']} | {row['seeded_per_run']} "
            f"| {row['delta']} | {row['first_witness']} |"
        )


if __name__ == "__main__":
    main()
