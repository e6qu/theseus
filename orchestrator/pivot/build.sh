#!/bin/bash
# Builds pivot.bin: the host-architecture static musl PID-1 injected into
# container-image VMs.
set -e

SOURCE=$(readlink -f "$0")
DIR="$(dirname "$SOURCE")"

case "$(uname -m)" in
    aarch64|arm64) target=aarch64-unknown-linux-musl ;;
    x86_64) target=x86_64-unknown-linux-musl ;;
    *) echo "unsupported pivot architecture: $(uname -m)" >&2; exit 1 ;;
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
cargo build --release --target "$target"
cp "target/$target/release/theseus-pivot" "$DIR/../pivot.bin"
echo "Built $DIR/../pivot.bin for $target"
