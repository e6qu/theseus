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

After the native certification workflow has run, each SHA release has one
fixed-plan certificate and one complete distributed-counterexample archive per
certified architecture:

```sh
gh release download "$TAG" --repo "$REPOSITORY" \
  --pattern "theseus-${TAG}-runtime-certificate-${ARCH}.json" \
  --pattern "theseus-${TAG}-multiservice-counterexample-${ARCH}.tar.gz" \
  --dir "$work"
for artifact in \
  "$work/theseus-${TAG}-runtime-certificate-${ARCH}.json" \
  "$work/theseus-${TAG}-multiservice-counterexample-${ARCH}.tar.gz"
do
  gh attestation verify "$artifact" --repo "$REPOSITORY" \
    --signer-workflow "$REPOSITORY/.github/workflows/certify-deterministic-runtime.yml"
done
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

`evidence/replay/` contains the certification run used to verify that replay.
`source/` contains the human-readable tutorial input, while `checkpoint/`
contains the locked runtime and workload artifacts. An absent
architecture-specific asset means that architecture has not been certified
for that SHA.

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
