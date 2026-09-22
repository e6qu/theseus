# Gate CI on a Theseus campaign

A complete recipe for running a Theseus campaign in GitHub Actions and
failing the workflow when a property is violated. The campaign runs on a
self-hosted Linux runner with KVM; the report and locked bundle upload as
artifacts so the failure is reproducible without re-running anything.

## 1. The runner

The job needs Linux with `/dev/kvm` and Docker. Label a self-hosted runner
`theseus-kvm`; the workflow below targets it directly. Nothing else is
required — the campaign runs from published Theseus artifacts, not a source
checkout.

## 2. The workflow

```yaml
name: theseus-campaign
on:
  pull_request:
  workflow_dispatch:

jobs:
  campaign:
    runs-on: [self-hosted, theseus-kvm]
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v4

      - name: Pull the pinned runtime
        env:
          THESEUS_IMAGE: ghcr.io/e6qu/theseus:SHA12CHARACTERS-amd64
        run: docker pull "$THESEUS_IMAGE"

      - name: Run the campaign
        working-directory: docs/tutorials/30-multiservice-lost-update
        env:
          THESEUS_IMAGE: ghcr.io/e6qu/theseus:SHA12CHARACTERS-amd64
        run: |
          docker run --rm --privileged --platform linux/amd64 \
            -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh -ec '
              theseus compose explore --max-runs 32 \
                --expect-counterexample lost_update_is_unreachable \
                --output campaign compose.yaml
            '

      - name: Publish the step summary and bundle
        if: always()
        working-directory: docs/tutorials/30-multiservice-lost-update
        env:
          THESEUS_IMAGE: ghcr.io/e6qu/theseus:SHA12CHARACTERS-amd64
        run: |
          docker run --rm --platform linux/amd64 \
            -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh -ec '
              theseus report --format github campaign >> "$GITHUB_STEP_SUMMARY"
              theseus report --format junit --output theseus-results.xml campaign
            '
          cp -a campaign theseus-results.xml "$RUNNER_TEMP"/ 2>/dev/null || true

      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: theseus-campaign
          path: |
            docs/tutorials/30-multiservice-lost-update/campaign
            docs/tutorials/30-multiservice-lost-update/theseus-results.xml
          retention-days: 30
```

## 3. What each piece gives you

- `--expect-counterexample` fails the step unless the named property is
  retained as failed; drop it to require the campaign to pass instead.
- The `github` report format emits `::error`/`::warning` annotations for
  every failed check, so the violating property and its first violating
  timeline appear directly on the workflow run summary. `theseus compare
  --format github` produces the same annotations for cross-run divergences,
  with both sides' moment addresses for temporal navigation.
- The uploaded bundle is the locked replay directory: anyone with the
  artifact can run the replay command printed in the report against the
  same published runtime, without re-running the exploration.
- `--max-runs` and `--guidance` pin the budget and search policy so CI
  results stay comparable across runs.
