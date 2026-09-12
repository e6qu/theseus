#!/bin/sh
set -eu

: "${THESEUS_TAG:?Set THESEUS_TAG to a published 12-character commit SHA.}"
image=${THESEUS_IMAGE:-ghcr.io/e6qu/theseus:$THESEUS_TAG}
name=theseus-compose-image-launch

trap 'rm -rf api/work worker/work campaign rerun' EXIT
mkdir -p api/work worker/work
docker build --platform linux/arm64 -t "$name-api" api
docker build --platform linux/arm64 -t "$name-worker" worker
docker save "$name-api" -o api/work/api.tar
docker save "$name-worker" -o worker/work/worker.tar

docker run --rm --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$image" sh ./run-in-runtime.sh
