#!/bin/sh
# Retain the side-by-side guidance comparison for one public Compose
# workload: explore the workload under every guidance mode at one fixed
# budget, keep each campaign, and emit the committed comparison artifact.
#
# Requires Linux with KVM and a theseus binary on PATH. Usage:
#   compare_guidance_modes.sh compose.yaml budget outdir
#
# The script is resumable: campaigns already present in outdir are kept, so
# a worker can restart or continue. The retained campaigns and
# comparison.md/comparison.json are the evidence; commit them beside the
# workload's evaluation.
set -e

compose=${1:?usage: compare_guidance_modes.sh compose.yaml budget outdir}
budget=${2:?usage: compare_guidance_modes.sh compose.yaml budget outdir}
outdir=${3:?usage: compare_guidance_modes.sh compose.yaml budget outdir}

mkdir -p "$outdir"
modes="coverage adaptive posterior property unified"
for mode in $modes; do
    if [ -d "$outdir/$mode" ]; then
        echo "keeping $outdir/$mode"
        continue
    fi
    theseus compose explore \
        --max-runs "$budget" \
        --guidance "$mode" \
        --output "$outdir/$mode" \
        "$compose"
done

paths=""
for mode in $modes; do
    paths="$paths $outdir/$mode"
done
# shellcheck disable=SC2086
theseus evaluate compare $paths --format json > "$outdir/comparison.json"
# shellcheck disable=SC2086
theseus evaluate compare $paths --format markdown > "$outdir/comparison.md"
echo "comparison: $outdir/comparison.md"
