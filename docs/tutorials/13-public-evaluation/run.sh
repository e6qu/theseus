#!/bin/sh
set -eu

theseus evaluate theseus-evaluation.toml > evaluation.json
grep -Fq 'theseus-public-evaluation-v1' evaluation.json
grep -Fq '"status": "passed"' evaluation.json
grep -Fq '"verified": 1' evaluation.json
grep -Fq '"counterexamples": 0' evaluation.json

theseus evaluate --format markdown theseus-evaluation.toml > evaluation.md
grep -Fq 'Replay and search evidence' evaluation.md
grep -Fq 'Conventional baseline' evaluation.md
grep -Fq 'informational; never a replay verdict' evaluation.md
echo 'PASS: Theseus summarized a versioned public evaluation without KVM'
