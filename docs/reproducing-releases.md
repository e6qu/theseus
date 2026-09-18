# Reproduce a runtime image

Start with a published SHA release. Its signed `build-inputs.json` records the
source commit, GitHub Action commits, Docker frontend and base-image digests,
Debian snapshots, kernel commit, source date, and the expected platform-image
digest.

Run these commands on a native Linux `amd64` or `arm64` Docker host. Set
`ARCH` to that host's architecture; do not cross-build for this check.

```sh
TAG=0123456789ab # replace with the release's 12-character SHA tag
ARCH=arm64       # or amd64
REPOSITORY=e6qu/theseus
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

gh release download "$TAG" --repo "$REPOSITORY" \
  --pattern "theseus-${TAG}-build-inputs.json" --dir "$work"
gh attestation verify "$work/theseus-${TAG}-build-inputs.json" \
  --repo "$REPOSITORY" \
  --signer-workflow "$REPOSITORY/.github/workflows/release.yml" \
  --source-ref refs/heads/main
```

Read the exact commit and build timestamp from the signed record, then build
without cache. The Dockerfile already pins every other runtime input.

```sh
inputs=$work/theseus-${TAG}-build-inputs.json
commit=$(jq -r .source.commit "$inputs")
epoch=$(jq -r .source.date_epoch "$inputs")
expected=$(jq -r ".runtime.platform_digests[\"$ARCH\"]" "$inputs")

git clone https://github.com/$REPOSITORY "$work/source"
git -C "$work/source" checkout --detach "$commit"
docker buildx build --platform "linux/$ARCH" --no-cache \
  --build-arg "SOURCE_DATE_EPOCH=$epoch" \
  --build-arg "THESEUS_SOURCE_COMMIT=$commit" \
  --output "type=oci,dest=$work/rebuilt.oci.tar,rewrite-timestamp=true" \
  "$work/source"
mkdir "$work/oci"
tar -xf "$work/rebuilt.oci.tar" -C "$work/oci"
actual=$(jq -r '.manifests[0].digest' "$work/oci/index.json")
test "$actual" = "$expected"
```

The final comparison establishes that the rebuilt platform image has the same OCI
manifest digest as the published `TAG-ARCH` runtime image. The on-demand
`verify runtime reproducibility` workflow performs this comparison twice with
independent no-cache builds for both architectures and signs a witness.

The runtime archive also contains `pivot` and `pivot.json`. The JSON identifies
the architecture, source commit, and SHA-256 of the exact PID-1 embedded by
`theseus-image`. Run `theseus-image pivot` to inspect the embedded copy and
compare its digest with the packaged file.

## Retrieve native KVM evidence

After native certification has completed, the SHA release has one fixed-plan
certificate, one complete distributed-counterexample archive, and one complete
runtime-validation archive for each architecture named by its index. Download
the index and its architecture assets:

```sh
gh release download "$TAG" --repo "$REPOSITORY" \
  --pattern "theseus-${TAG}-native-evidence.json" \
  --pattern "theseus-${TAG}-runtime-certificate-*.json" \
  --pattern "theseus-${TAG}-multiservice-counterexample-*.tar.gz" \
  --pattern "theseus-${TAG}-runtime-validation-*.tar.gz" \
  --dir "$work"
for artifact in "$work"/theseus-${TAG}-native-evidence.json \
  "$work"/theseus-${TAG}-runtime-certificate-*.json \
  "$work"/theseus-${TAG}-multiservice-counterexample-*.tar.gz \
  "$work"/theseus-${TAG}-runtime-validation-*.tar.gz
do
  gh attestation verify "$artifact" --repo "$REPOSITORY" \
    --signer-workflow "$REPOSITORY/.github/workflows/certify-deterministic-runtime.yml"
done
```

Run the portable verifier from the same SHA release. It rejects a missing
indexed architecture, a renamed or changed asset, a mismatched certificate, an
unsafe archive, a changed file inventory, a replay that lacks the required
partition, dropped frame, recovery probe, or lost update, and validation that
lacks its container, coverage, schedule-search, pthread, or ordered-execution
evidence. It also rejects partial per-run ledgers, malformed machine traces,
malformed or contradictory comparisons, and empty evaluations. A well-formed
`diverged` comparison remains valid observational evidence; the explicit replay
checks decide replay admission:

```sh
theseus evidence verify "$work/theseus-${TAG}-native-evidence.json"
```

Extract the counterexample archive and enter its `minimized` directory. Inspect
`evidence/proof.json`, `evidence/campaign-result.json`, and
`minimization.json`, then reproduce the portable locked plan:

```sh
tar -xzf "$work/theseus-${TAG}-multiservice-counterexample-${ARCH}.tar.gz" -C "$work"
cd "$work/minimized"
docker run --rm --privileged --platform "linux/$ARCH" \
  -v "$PWD":/proof -w /proof \
  "ghcr.io/e6qu/theseus:${TAG}-${ARCH}" \
  theseus compose replay . --output reproduced
```

Extract the validation archive to inspect its five additional product paths:

```sh
tar -xzf "$work/theseus-${TAG}-runtime-validation-${ARCH}.tar.gz" -C "$work"
find "$work/validation" -name plan.json -o -name campaign-result.json \
  -o -name result.json -o -name report.md -o -name minimization.json
```

`validation/evidence.json` records the native architecture, host kernel, KVM
API, digest-pinned runtime, scenario list, and every retained file's size and
SHA-256. The five directories contain the locked container run, coverage
campaign, minimized schedule-search counterexample, pthread campaign, ordered
KVM-exit campaign, reports, comparisons, offline evaluation, and checked
replays. Those outputs establish that the CLI shipped in the release performed
its inspect, compare, evaluate, minimize, and replay workflow against the same
retained corpus.
Each scenario's `source/` directory keeps the small Dockerfile, manifest,
Compose file, and C program needed to understand the locked workload.

`evidence/runtime-certificate.json` is byte-for-byte identical to the separate
certificate asset. The certificate embeds the exact fixed plan covered by its
plan digest. `evidence/replay/` contains the certification run used to verify
that replay.
`source/` contains the human-readable tutorial input, while `checkpoint/`
contains the locked runtime and workload artifacts. An absent
architecture-specific asset or the pair index means that the SHA does not have
a native certification for that architecture. Each certification run verifies
its selected architecture set with the released CLI, attests every asset, and
only then uploads it to the release. A successful SHA release starts amd64
certification automatically; arm64 requires a native arm64 KVM runner.

For Tutorial 30 evidence, `minimization.json` must retain both
`backplane:partition@setup` and `backplane:heal@probe_partition`.
`evidence/replay/topology-result.json` records the applied actions, and the
network evidence records a dropped frame. The writer serial log records the
successful post-recovery HTTP probe. These checks separate the network
recovery path from the later concurrency-dependent lost update.

## Create an external witness

Run that workflow in a GitHub repository you control (a fork is fine). Pass
the official repository and a published tag. It verifies the official signed
input record, checks out its exact commit, performs two clean native builds,
compares both OCI layouts and the published digest, then signs the resulting
`witness.json` and retains it as an Actions artifact for 90 days.

```sh
gh workflow run verify-runtime-reproducibility.yml \
  --repo YOUR_ACCOUNT/theseus \
  --ref main \
  -f repository=e6qu/theseus \
  -f tag="$TAG"
```

The witness attestation belongs to the repository that ran the workflow. That
separates the external observer's signed rebuild record from Theseus's release
attestation.
