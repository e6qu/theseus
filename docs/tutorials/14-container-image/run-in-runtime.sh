#!/bin/sh
set -eu

mkdir -p work/runtime work/guest
cp /usr/local/bin/firecracker work/runtime/firecracker
cp /usr/local/bin/theseus-image work/runtime/theseus-image
cp /opt/theseus/vmlinux work/guest/vmlinux
theseus test --output work/replay theseus.toml
grep -a '^THES:HTTP:operation:read_health:PASS$' work/replay/serial.log
theseus replay work/replay
echo 'PASS: Theseus checked and replayed an unmodified container service'
