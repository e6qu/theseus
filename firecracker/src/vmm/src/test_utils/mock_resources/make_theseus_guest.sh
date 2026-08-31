#!/bin/bash
# Builds theseus_guest.bin (aarch64 bare-metal test guest) from theseus_guest.S.
# Follows the make_noisy_kernel.sh pattern. Requires an aarch64 toolchain
# (native on aarch64 hosts/containers, or cross via gcc-aarch64-linux-gnu).

set -e

SOURCE=$(readlink -f "$0")
DIR="$(dirname "$SOURCE")"

if command -v aarch64-linux-gnu-as >/dev/null 2>&1; then
    AS=aarch64-linux-gnu-as
    LD=aarch64-linux-gnu-ld
elif [ "$(uname -m)" = "aarch64" ]; then
    AS=as
    LD=ld
else
    echo "need an aarch64 assembler" >&2
    exit 1
fi

"$AS" -o /tmp/theseus_guest.o "$DIR/theseus_guest.S"
"$LD" -o /tmp/theseus_guest.elf /tmp/theseus_guest.o
# Flat binary: the loader places it at DRAM start; entry at offset 0.
aarch64-linux-gnu-objcopy -O binary /tmp/theseus_guest.elf "$DIR/theseus_guest.bin" 2>/dev/null \
    || objcopy -O binary /tmp/theseus_guest.elf "$DIR/theseus_guest.bin"

echo "Built $DIR/theseus_guest.bin"
