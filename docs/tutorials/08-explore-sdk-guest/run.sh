#!/bin/sh
set -eu

[ -f guest.bin ] && [ -x /usr/local/bin/theseus ] && [ -x /usr/local/bin/firecracker ] || {
    echo 'Build guest.bin, then run this in a published Theseus runtime image.' >&2
    exit 1
}

mkdir -p runtime initramfs-root
cp /usr/local/bin/firecracker runtime/firecracker
(cd initramfs-root && find . -print | cpio -o -H newc --quiet > ../empty-initramfs.cpio)

theseus explore
grep -a '"status": "passed"' theseus-exploration/result.json
grep -a '"seed_path"' theseus-exploration/result.json
echo 'PASS: Theseus explored the recorded SDK guest timelines'
