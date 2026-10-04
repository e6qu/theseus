#!/usr/bin/env python3
"""Compare a seeded rerun's retained run sequence with its seed campaign.

Reads both campaigns' result JSON, prints the per-run choice sequences,
and writes a versioned comparison record beside them. Run from the
repository root; usage:

    seeded_rerun_comparison.py seed-campaign rerun-campaign output-dir
"""

import json
import sys
from pathlib import Path


def sequence(campaign: Path) -> list[tuple]:
    result = json.loads((campaign / "campaign-result.json").read_text())
    sequence = []
    for run in result["runs"]:
        picks = []
        for choice in run.get("structured_choices", {}).get("chooser", []):
            picks.append(f'{choice["name"]}={choice["selected"]}')
        sequence.append(tuple(picks))
    return sequence


def main() -> None:
    seed = Path(sys.argv[1])
    rerun = Path(sys.argv[2])
    outdir = Path(sys.argv[3])

    first = sequence(seed)
    second = sequence(rerun)
    print("seed sequence:", first)
    print("seeded rerun sequence:", second)
    outdir.mkdir(parents=True, exist_ok=True)
    (outdir / "sequence-comparison.json").write_text(json.dumps({
        "format": "theseus-seeded-rerun-comparison-v1",
        "seed": [list(run) for run in first],
        "seeded_rerun": [list(run) for run in second],
    }, indent=1) + "\n")


if __name__ == "__main__":
    main()
