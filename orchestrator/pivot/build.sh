#!/bin/bash
# Build the static Linux PID-1 embedded by theseus-image. The output target is
# explicit so release and verification jobs cannot accidentally package a
# pivot for the runner's other architecture.
set -eu

DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target=${THESEUS_PIVOT_TARGET:-}
output=${THESEUS_PIVOT_OUTPUT:-$DIR/../pivot.bin}

if [ "$#" -gt 0 ]; then
    if [ "$#" -ne 2 ] || [ "$1" != "--target" ]; then
        echo "usage: build.sh [--target amd64|arm64]" >&2
        exit 2
    fi
    case "$2" in
        amd64) target=x86_64-unknown-linux-musl ;;
        arm64) target=aarch64-unknown-linux-musl ;;
        *) echo "unsupported pivot target: $2" >&2; exit 2 ;;
    esac
fi

if [ -z "$target" ]; then
    case "$(uname -m)" in
        aarch64|arm64) target=aarch64-unknown-linux-musl ;;
        x86_64) target=x86_64-unknown-linux-musl ;;
        *) echo "unsupported pivot architecture: $(uname -m)" >&2; exit 1 ;;
    esac
fi

case "$target" in
    amd64) target=x86_64-unknown-linux-musl ;;
    arm64) target=aarch64-unknown-linux-musl ;;
esac

case "$target" in
    aarch64-unknown-linux-musl) architecture=arm64 ;;
    x86_64-unknown-linux-musl) architecture=amd64 ;;
    *) echo "unsupported pivot Rust target: $target" >&2; exit 2 ;;
esac

# macOS's system linker cannot link a Linux musl executable. rustc can use its
# bundled linker for this fully static pivot.
if [ "$(uname -s)" = Darwin ]; then
    case "$target" in
        aarch64-unknown-linux-musl) export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld ;;
        x86_64-unknown-linux-musl) export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld ;;
    esac
fi

cd "$DIR"
export CARGO_TARGET_DIR="$DIR/target"
rustup target add "$target"
cargo build --release --locked --target "$target"
cp "target/$target/release/theseus-pivot" "$output"

description=$(file -b "$output")
case "$architecture:$description" in
    amd64:*x86-64*) ;;
    arm64:*ARM\ aarch64*) ;;
    *) echo "pivot architecture mismatch: expected $architecture, got $description" >&2; exit 1 ;;
esac
case "$description" in
    *statically\ linked*|*static-pie\ linked*) ;;
    *) echo "pivot must be statically linked: $description" >&2; exit 1 ;;
esac

echo "Built $output for $target"
