#!/bin/bash
# Builds pivot.bin: the host-architecture static musl PID-1 injected into
# container-image VMs.
set -e

SOURCE=$(readlink -f "$0")
DIR="$(dirname "$SOURCE")"

case "$(uname -m)" in
    aarch64) target=aarch64-unknown-linux-musl ;;
    x86_64) target=x86_64-unknown-linux-musl ;;
    *) echo "unsupported pivot architecture: $(uname -m)" >&2; exit 1 ;;
esac

cd "$DIR"
export CARGO_TARGET_DIR="$DIR/target"
rustup target add "$target"
cargo build --release --target "$target"
cp "target/$target/release/theseus-pivot" "$DIR/../pivot.bin"
echo "Built $DIR/../pivot.bin for $target"
