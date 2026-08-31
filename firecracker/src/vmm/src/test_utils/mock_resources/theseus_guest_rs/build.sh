#!/bin/bash
# Builds theseus_guest_rs.bin from the theseus-guest-rs crate (bare-metal
# aarch64, Rust + theseus-sdk). Requires the aarch64-unknown-none target and
# binutils (objcopy).

set -e

SOURCE=$(readlink -f "$0")
DIR="$(dirname "$SOURCE")"

cd "$DIR"
# The Firecracker repo's .cargo/config.toml redirects target-dir; pin ours.
export CARGO_TARGET_DIR="$DIR/target"
cargo build --release
# Flat binary at DRAM start (0x80000000), as the linux-loader PE/Image path
# expects.
objcopy -O binary target/aarch64-unknown-none/release/theseus-guest-rs \
    "$DIR/../theseus_guest_rs.bin"

echo "Built $DIR/../theseus_guest_rs.bin"
