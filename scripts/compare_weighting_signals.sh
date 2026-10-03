#!/bin/sh
# Rank the guidance-weighting signals the design note names: explore one
# public Compose workload seeded and unseeded at one fixed budget, keep
# both campaigns, and tabulate property yield per run plus the
# choice-outcome shares each signal reads.
#
# Requires Linux with KVM, a theseus binary on PATH, and a workload whose
# campaign declares bounded choices with a property (see tutorial 36).
# Usage:
#   compare_weighting_signals.sh compose.yaml budget outdir property
#
# The property is asserted with --expect-counterexample in both
# explorations, so a retained counterexample counts as success.
#
# Resumable like compare_guidance_modes.sh: existing campaigns are kept.
# The retained campaigns and signals.md are the evidence; a signal ships
# into the policy only when this comparison shows it finds failures or
# witnesses sooner than first-seen novelty alone.
set -e

compose=${1:?usage: compare_weighting_signals.sh compose.yaml budget outdir}
budget=${2:?usage: compare_weighting_signals.sh compose.yaml budget outdir}
outdir=${3:?usage: compare_weighting_signals.sh compose.yaml budget outdir}

mkdir -p "$outdir"

property=${4:?usage: compare_weighting_signals.sh compose.yaml budget outdir property}

if [ -d "$outdir/unseeded" ]; then
    echo "keeping $outdir/unseeded"
else
    theseus compose explore \
        --expect-counterexample "$property" \
        --output "$outdir/unseeded" \
        --max-runs "$budget" \
        --guidance unified \
        "$compose"
fi

if [ -d "$outdir/seeded" ]; then
    echo "keeping $outdir/seeded"
else
    # The seeded run continues from the unseeded campaign's consumed
    # identities, so it prefers values the first exploration used.
    theseus compose explore \
        --expect-counterexample "$property" \
        --output "$outdir/seeded" \
        --max-runs "$budget" \
        --guidance unified \
        --seed-choices "$outdir/unseeded" \
        "$compose"
fi

theseus evaluate compare \
    "$outdir/unseeded" "$outdir/seeded" \
    --format json > "$outdir/signals.json"
theseus evaluate compare \
    "$outdir/unseeded" "$outdir/seeded" \
    --format markdown > "$outdir/signals.md"

theseus history "$outdir/unseeded" "$outdir/seeded" \
    --choices --format json > "$outdir/choices.json"

# Tabulate the experiment: per arm, runs, failed runs, property
# witnesses, and witnesses per run; plus the top failed-run shares per
# consumed identity. The ranking is a row comparison, not hand-read JSON.
python3 - "$outdir" <<'TABULATE'
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

echo "campaigns:   $outdir/unseeded $outdir/seeded"
echo "comparison:  $outdir/signals.md"
echo "choice catalog: $outdir/choices.json"
echo
echo "The tabulation is the experiment the design note requires before any"
echo "weighting signal ships into the policy."
