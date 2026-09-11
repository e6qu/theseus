#!/bin/sh
set -eu

theseus compare campaign-before campaign-after > comparison.json
grep -Fq 'first operation-boundary state differs' comparison.json
grep -Fq '"boundary": 1' comparison.json

theseus compare --format markdown campaign-before campaign-after > investigation.md
grep -Fq 'First causal divergence' investigation.md
grep -Fq 'topology state differs' investigation.md

theseus compare --query /runs/0/timeline/1/program_counters \
  campaign-before campaign-after > coverage.json
grep -Fq '0x8010' coverage.json
grep -Fq '0x8020' coverage.json

theseus compare --query /properties/0/status campaign-before campaign-after > properties.json
grep -Fq '"passed"' properties.json
grep -Fq '"failed"' properties.json
echo 'PASS: Theseus explained the first retained campaign divergence'
