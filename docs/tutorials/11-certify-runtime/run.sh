#!/bin/sh
set -eu

[ -x /usr/local/bin/theseus ] && [ -x /usr/local/bin/theseus-topology ] && [ -x /usr/local/bin/firecracker ] && [ -f /opt/theseus/vmlinux ] || {
    echo 'Run this tutorial in a published Theseus runtime image.' >&2
    exit 1
}

mkdir -p service/runtime service/guest/root/bin
cp /usr/local/bin/firecracker service/runtime/firecracker
cp /opt/theseus/vmlinux service/guest/vmlinux
cp /bin/busybox service/guest/root/bin/busybox
for applet in mount sleep poweroff; do
    ln -s busybox "service/guest/root/bin/$applet"
done
cp service/init service/guest/root/init
chmod +x service/guest/root/init
(cd service/guest/root && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)

theseus compose plan > plan.json
theseus-topology certify --plan plan.json --output certificate
grep -F '"status": "passed"' certificate/certificate.json
grep -F '"executions": 2' certificate/certificate.json
grep -F '"name": "replay_entropy"' certificate/replay/services/service/result.json
echo 'PASS: Theseus certified two exact KVM executions'
