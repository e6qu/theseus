#!/bin/sh
set -eu

mkdir -p api/work/runtime api/work/guest
cp /usr/local/bin/firecracker api/work/runtime/firecracker
cp /usr/local/bin/theseus-image api/work/runtime/theseus-image
cp /opt/theseus/vmlinux api/work/guest/vmlinux

theseus compose explore --output campaign compose.yaml
theseus evaluate capture campaign --output public-evaluation --name "HTTP health campaign"
theseus evaluate public-evaluation/theseus-evaluation.toml > evaluation.json
grep -Fq '"status": "passed"' evaluation.json
grep -Fq '"files":' evaluation.json
theseus compose replay public-evaluation/campaign --output rerun
grep -a '^THES:HTTP:operation:read_health:PASS$' rerun/services/api/serial.log
echo 'PASS: Theseus published and replayed a locked public campaign'
