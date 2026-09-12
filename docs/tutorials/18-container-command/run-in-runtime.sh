#!/bin/sh
set -eu

mkdir -p api/work/runtime api/work/guest
cp /usr/local/bin/firecracker api/work/runtime/firecracker
cp /usr/local/bin/theseus-image api/work/runtime/theseus-image
cp /opt/theseus/vmlinux api/work/guest/vmlinux

theseus compose explore --output campaign compose.yaml
grep -a '^THES:SHELL:operation:read_health_file:PASS$' campaign/services/api/serial.log
grep -a '"name": "command_reported_ok"' campaign/campaign-result.json
theseus compose replay campaign --output rerun
grep -a '^THES:SHELL:operation:read_health_file:PASS$' rerun/services/api/serial.log
echo 'PASS: Theseus campaigned a command in an unmodified container image'
