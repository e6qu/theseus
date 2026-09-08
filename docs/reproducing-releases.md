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
  --output "type=oci,dest=$work/rebuilt.oci.tar,rewrite-timestamp=true" \
  "$work/source"
mkdir "$work/oci"
tar -xf "$work/rebuilt.oci.tar" -C "$work/oci"
actual=$(jq -r '.manifests[0].digest' "$work/oci/index.json")
test "$actual" = "$expected"
```

The final comparison proves that the rebuilt platform image has the same OCI
manifest digest as the published `TAG-ARCH` runtime image. The on-demand
`verify runtime reproducibility` workflow performs this comparison twice with
independent no-cache builds for both architectures and signs a witness.

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
