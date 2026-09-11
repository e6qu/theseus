#!/bin/sh
set -eu

mkdir -p work/runtime work/guest
cp /usr/local/bin/firecracker work/runtime/firecracker
cp /usr/local/bin/theseus-image work/runtime/theseus-image
cp /opt/theseus/vmlinux work/guest/vmlinux
theseus test --output work/replay theseus.toml
grep -a '^container image booted$' work/replay/serial.log
theseus replay work/replay
echo 'PASS: Theseus converted and replayed an unmodified container image'
