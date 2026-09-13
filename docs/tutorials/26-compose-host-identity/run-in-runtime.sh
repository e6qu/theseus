#!/bin/sh
set -eu

mkdir -p api/work/runtime api/work/guest
cp /usr/local/bin/firecracker api/work/runtime/firecracker
cp /usr/local/bin/theseus-image api/work/runtime/theseus-image
cp /opt/theseus/vmlinux api/work/guest/vmlinux
theseus compose explore --output campaign compose.yaml
grep -a '^THES:SHELL:operation:inspect_host_identity:PASS$' campaign/services/api/serial.log
theseus compose replay campaign --output rerun
grep -a '^THES:SHELL:operation:inspect_host_identity:PASS$' rerun/services/api/serial.log
echo 'PASS: Theseus locked Compose hostname and extra_hosts into an image service'
