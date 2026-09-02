#!/bin/sh
set -eu

[ -x /usr/local/bin/theseus ] && [ -x /usr/local/bin/firecracker ] && [ -f /opt/theseus/vmlinux ] || {
    echo 'Run this tutorial in a published Theseus runtime image.' >&2
    exit 1
}

for service in api worker; do
    mkdir -p "$service/runtime" "$service/guest/root/bin"
    cp /usr/local/bin/firecracker "$service/runtime/firecracker"
    cp /opt/theseus/vmlinux "$service/guest/vmlinux"
    cp /bin/busybox "$service/guest/root/bin/busybox"
    for applet in mount ip sleep ping poweroff; do
        ln -s busybox "$service/guest/root/bin/$applet"
    done
    cp "$service/init" "$service/guest/root/init"
    chmod +x "$service/guest/root/init"
    (cd "$service/guest/root" && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
done

theseus compose test
grep -a '^ping passed$' theseus-compose-replay/services/api/serial.log
grep -a '^serial command accepted$' theseus-compose-replay/services/api/serial.log
theseus compose replay theseus-compose-replay --output topology-replay
grep -a 'serial logs match the original replay bundle' topology-replay/services/api/result.json
grep -a 'network topology matches the original replay bundle' topology-replay/services/api/result.json
grep -a 'simulated network traffic matches the original replay bundle' topology-replay/services/api/result.json
echo 'PASS: api read deterministic UART input and reached worker'
