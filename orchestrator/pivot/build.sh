#!/bin/bash
# Builds pivot.bin: the static musl PID-1 injected into container-image VMs.
set -e

SOURCE=$(readlink -f "$0")
DIR="$(dirname "$SOURCE")"

cd "$DIR"
export CARGO_TARGET_DIR="$DIR/target"
cargo build --release --target aarch64-unknown-linux-musl
cp target/aarch64-unknown-linux-musl/release/theseus-pivot "$DIR/../pivot.bin"
echo "Built $DIR/../pivot.bin"
