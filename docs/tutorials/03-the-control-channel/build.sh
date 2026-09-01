#!/bin/sh
set -eu

: "${THESEUS_TAG:?Set THESEUS_TAG to a published 12-character commit SHA.}"
url="https://github.com/e6qu/theseus/releases/download/$THESEUS_TAG/theseus-sdk-0.1.0.crate"

rm -rf vendor guest.bin target
mkdir vendor
curl -fsSL "$url" | tar -xz -C vendor --strip-components=1
cargo build --release
objcopy -O binary target/aarch64-unknown-none/release/theseus-sdk-tutorial guest.bin
echo 'Built guest.bin'
