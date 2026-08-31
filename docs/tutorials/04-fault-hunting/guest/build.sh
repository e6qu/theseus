#!/bin/bash
# Builds counter_guest.bin from the counter-guest crate (bare-metal aarch64).
set -e

SOURCE=$(readlink -f "$0")
DIR="$(dirname "$SOURCE")"

cd "$DIR"
export CARGO_TARGET_DIR="$DIR/target"
cargo build --release
objcopy -O binary target/aarch64-unknown-none/release/counter-guest \
    "$DIR/counter_guest.bin"

echo "Built $DIR/counter_guest.bin"
