# Tutorial 12: Compare two campaign bundles

Find the first retained difference between two completed campaign fixtures and
query the evidence at that boundary. This is offline comparison, not
counterfactual causality analysis.

Run every command from this directory with a published `theseus` binary on
`PATH`. No KVM, Docker, snapshot, or source checkout is needed.

## 1. Compare the recorded histories

```sh
theseus compare campaign-before campaign-after > comparison.json
grep -F 'first operation-boundary state differs' comparison.json
grep -F '"boundary": 1' comparison.json
```

The result identifies the first operation boundary at which these two retained
histories differ. It does not prove that the changed field caused a failure.

## 2. Render a Markdown investigation note

```sh
theseus compare --format markdown campaign-before campaign-after > investigation.md
grep -F 'First recorded divergence' investigation.md
grep -F 'topology state differs' investigation.md
```

## 3. Query exact retained fields

```sh
theseus compare --query /runs/0/timeline/1/program_counters \
  campaign-before campaign-after > coverage.json
grep -F '0x8010' coverage.json
grep -F '0x8020' coverage.json
theseus compare --query /properties/0/status \
  campaign-before campaign-after > properties.json
grep -F '"passed"' properties.json
grep -F '"failed"' properties.json
```

The `program_counters` field contains sampled vCPU instruction pointers. It is
not application basic-block coverage.

## 4. Clean up (optional)

```sh
rm -f comparison.json investigation.md coverage.json properties.json
```
