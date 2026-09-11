#!/bin/sh
set -eu

: "${THESEUS_TAG:?Set THESEUS_TAG to a published 12-character commit SHA.}"
image=${THESEUS_IMAGE:-ghcr.io/e6qu/theseus:$THESEUS_TAG}
name=theseus-public-campaign-tutorial

trap 'rm -rf api/work campaign public-evaluation rerun evaluation.json' EXIT
mkdir -p api/work
docker build --platform linux/arm64 -t "$name" .
docker save "$name" -o api/work/service.tar

docker run --rm --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$image" sh ./run-in-runtime.sh
