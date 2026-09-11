#!/bin/sh
# Run this file from this directory. Docker builds the service image; the
# published Theseus runtime converts and runs it.
set -eu

: "${THESEUS_TAG:?Set THESEUS_TAG to a published 12-character commit SHA.}"
image=${THESEUS_IMAGE:-ghcr.io/e6qu/theseus:$THESEUS_TAG}
work=work
name=theseus-container-image-tutorial

trap 'rm -rf "$work"' EXIT
mkdir "$work"
docker build --platform linux/arm64 -t "$name" .
docker save "$name" -o "$work/service.tar"

docker run --rm --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$image" sh ./run-in-runtime.sh
