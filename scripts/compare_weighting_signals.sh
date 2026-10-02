#!/bin/sh
# Rank the guidance-weighting signals the design note names: explore one
# public Compose workload seeded and unseeded at one fixed budget, keep
# both campaigns, and tabulate property yield per run plus the
# choice-outcome shares each signal reads.
#
# Requires Linux with KVM, a theseus binary on PATH, and a workload whose
# campaign declares bounded choices with a property (see tutorial 36).
# Usage:
#   compare_weighting_signals.sh compose.yaml budget outdir
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

if [ -d "$outdir/unseeded" ]; then
    echo "keeping $outdir/unseeded"
else
    theseus compose explore \
        --max-runs "$budget" \
        --guidance unified \
        --output "$outdir/unseeded" \
        "$compose"
fi

if [ -d "$outdir/seeded" ]; then
    echo "keeping $outdir/seeded"
else
    # The seeded run continues from the unseeded campaign's consumed
    # identities, so it prefers values the first exploration used.
    theseus compose explore \
        --max-runs "$budget" \
        --guidance unified \
        --seed-choices "$outdir/unseeded" \
        --output "$outdir/seeded" \
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

echo "campaigns:   $outdir/unseeded $outdir/seeded"
echo "comparison:  $outdir/signals.md"
echo "choice catalog: $outdir/choices.json"
echo
echo "property yield per run (from signals.json rows: failed properties"
echo "and failed runs), and per-identity outcome shares (from"
echo "choices.json): the experiment the design note requires before any"
echo "weighting signal ships into the policy."
