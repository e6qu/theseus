# Reproduce a runtime image

Start with a published SHA release. Its signed `build-inputs.json` records the
source commit, Docker frontend and base-image digests, Debian snapshots, kernel
commit, source date, and the expected platform-image digest.

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
independent no-cache builds for both architectures.
