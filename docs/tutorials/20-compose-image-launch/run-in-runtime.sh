#!/bin/sh
set -eu

for service in api worker; do
  mkdir -p "$service/work/runtime" "$service/work/guest"
  cp /usr/local/bin/firecracker "$service/work/runtime/firecracker"
  cp /usr/local/bin/theseus-image "$service/work/runtime/theseus-image"
  cp /opt/theseus/vmlinux "$service/work/guest/vmlinux"
done

theseus compose explore --output campaign compose.yaml
grep -a '^THES:SHELL:operation:read_alternate_identity:PASS$' campaign/services/api/serial.log
grep -a 'alternate directory from Compose' campaign/services/api/serial.log
theseus compose replay campaign --output rerun
grep -a '^THES:SHELL:operation:read_alternate_identity:PASS$' rerun/services/api/serial.log
echo 'PASS: Theseus locked the Compose image launch contract'
