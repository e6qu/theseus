#!/bin/sh
# Rank the guidance-weighting signals the design note names: explore one
# public Compose workload seeded and unseeded at one fixed budget, keep
# both campaigns, and tabulate property yield per run plus the
# choice-outcome shares each signal reads.
#
# Requires Linux with KVM, a theseus binary on PATH, and a workload whose
# campaign declares bounded choices with a property (see tutorial 36).
# Usage:
#   compare_weighting_signals.sh budget outdir property compose.yaml [compose.yaml...]
#
# Runs the seeded/unseeded pair for EVERY named workload, nesting each
# under outdir/<workload-name>, and tabulates per workload. The property
# is asserted with --expect-counterexample in both explorations, so a
# retained counterexample counts as success.
#
# Resumable like compare_guidance_modes.sh: existing campaigns are kept.
# The retained campaigns and per-workload tabulations are the evidence;
# a signal ships into the policy only when this comparison shows it
# finds failures or witnesses sooner than first-seen novelty alone.
set -e

budget=${1:?usage: compare_weighting_signals.sh budget outdir property compose.yaml [compose.yaml...]}
outdir=${2:?usage: compare_weighting_signals.sh budget outdir property compose.yaml [compose.yaml...]}
property=${3:?usage: compare_weighting_signals.sh budget outdir property compose.yaml [compose.yaml...]}
shift 3
[ $# -ge 1 ] || { echo "usage: compare_weighting_signals.sh budget outdir property compose.yaml [compose.yaml...]" >&2; exit 2; }

mkdir -p "$outdir"

for compose in "$@"; do
    name=$(basename "$compose" .yaml)
    work="$outdir/$name"

    if [ -d "$work/unseeded" ]; then
        echo "keeping $work/unseeded"
    else
        theseus compose explore \
            --expect-counterexample "$property" \
            --output "$work/unseeded" \
            --max-runs "$budget" \
            --guidance unified \
            "$compose"
    fi

    if [ -d "$work/seeded" ]; then
        echo "keeping $work/seeded"
    else
        # The seeded run continues from the unseeded campaign's consumed
        # identities, so it prefers values the first exploration used.
        theseus compose explore \
            --expect-counterexample "$property" \
            --output "$work/seeded" \
            --max-runs "$budget" \
            --guidance unified \
            --seed-choices "$work/unseeded" \
            "$compose"
    fi

    theseus evaluate compare \
        "$work/unseeded" "$work/seeded" \
        --format json > "$work/signals.json"
    theseus evaluate compare \
        "$work/unseeded" "$work/seeded" \
        --format markdown > "$work/signals.md"

    theseus history "$work/unseeded" "$work/seeded" \
        --choices --format json > "$work/choices.json"

    # Tabulate the experiment: per arm, runs, failed runs, property
    # witnesses, and witnesses per run; plus the top failed-run shares per
    # consumed identity. The ranking is a row comparison, not hand-read JSON.
    python3 - "$work" <<'TABULATE'
import json
import sys
from pathlib import Path

outdir = Path(sys.argv[1])
rows = []
for arm in ["unseeded", "seeded"]:
    result = json.loads((outdir / arm / "campaign-result.json").read_text())
    runs = result.get("runs", [])
    total = len(runs)
    failed = sum(1 for run in runs if run.get("status") == "failed")
    witnesses = sum(len(run.get("property_witnesses", [])) for run in runs)
    per_run = witnesses / total if total else 0.0
    rows.append((arm, total, failed, witnesses, per_run))

catalog = json.loads((outdir / "choices.json").read_text())
shares = sorted(
    catalog.get("choices", []),
    key=lambda entry: (
        entry.get("total_failed_runs", 0) / entry["total_runs"]
        if entry.get("total_runs")
        else 0,
        -entry.get("total_runs", 0),
    ),
    reverse=True,
)

lines = [
    "| arm | runs | failed runs | property witnesses | witnesses per run |",
    "| --- | --- | --- | --- | --- |",
]
for arm, total, failed, witnesses, per_run in rows:
    lines.append(
        f"| {arm} | {total} | {failed} | {witnesses} | {per_run:.2f} |"
    )
lines += [
    "",
    "Top failed-run shares per consumed identity (choice / failed / runs):",
]
for entry in shares[:5]:
    lines.append(
        f"- {entry['choice']}: {entry['total_failed_runs']}"
        f"/{entry['total_runs']}"
    )
(outdir / "tabulation.md").write_text("\n".join(lines) + "\n")
print(f"tabulation: {outdir/'tabulation.md'}")
TABULATE
done

# Summary across workloads: one row per workload per arm.
python3 - "$outdir" <<'SUMMARIZE'
import json
import sys
from pathlib import Path

outdir = Path(sys.argv[1])
lines = [
    "| workload | arm | runs | failed runs | property witnesses | witnesses per run |",
    "| --- | --- | --- | --- | --- | --- |",
]
for work in sorted(outdir.iterdir()):
    if not work.is_dir():
        continue
    for arm in ["unseeded", "seeded"]:
        result_path = work / arm / "campaign-result.json"
        if not result_path.is_file():
            continue
        result = json.loads(result_path.read_text())
        runs = result.get("runs", [])
        total = len(runs)
        failed = sum(1 for run in runs if run.get("status") == "failed")
        witnesses = sum(len(run.get("property_witnesses", [])) for run in runs)
        per_run = witnesses / total if total else 0.0
        lines.append(
            f"| {work.name} | {arm} | {total} | {failed} | {witnesses} | {per_run:.2f} |"
        )
summary = outdir / "summary.md"
summary.write_text(
    "# Weighting-signal summary across workloads\n\n"
    + "\n".join(lines)
    + "\n"
)
print(f"summary: {summary}")
SUMMARIZE
