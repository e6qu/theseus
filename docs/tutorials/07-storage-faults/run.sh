#!/bin/sh
set -eu

[ -x /usr/local/bin/theseus ] && [ -x /usr/local/bin/firecracker ] && [ -f /opt/theseus/vmlinux ] || {
    echo 'Run this tutorial in a published Theseus runtime image.' >&2
    exit 1
}

mkdir -p service/runtime service/guest/root/bin
cp /usr/local/bin/firecracker service/runtime/firecracker
cp /opt/theseus/vmlinux service/guest/vmlinux
cp /bin/busybox service/guest/root/bin/busybox
for applet in mkdir mount dd cmp poweroff; do
    ln -s busybox "service/guest/root/bin/$applet"
done
cp service/init service/guest/root/init
chmod +x service/guest/root/init
(cd service/guest/root && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)

theseus compose test
grep -a '^storage fault observed$' theseus-compose-replay/services/service/serial.log
echo 'PASS: Theseus injected the recorded storage fault'
