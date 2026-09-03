#!/bin/sh
set -eu

[ -x /usr/local/bin/theseus ] && [ -x /usr/local/bin/firecracker ] && [ -f /opt/theseus/vmlinux ] || {
    echo 'Run this tutorial in a published Theseus runtime image.' >&2
    exit 1
}

for service in api replica auditor; do
    mkdir -p "$service/runtime" "$service/guest/root/bin"
    cp /usr/local/bin/firecracker "$service/runtime/firecracker"
    cp /opt/theseus/vmlinux "$service/guest/vmlinux"
    cp /bin/busybox "$service/guest/root/bin/busybox"
    for applet in mount sleep poweroff; do
        ln -s busybox "$service/guest/root/bin/$applet"
    done
    cp "$service/init" "$service/guest/root/init"
    chmod +x "$service/guest/root/init"
    (cd "$service/guest/root" && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
done

if theseus compose explore --output theseus-compose-campaign; then
    echo 'expected the intentional stale-read property failure' >&2
    exit 1
fi

grep -a '"name": "consistent_read"' theseus-compose-campaign/campaign-result.json
grep -a '"status": "failed"' theseus-compose-campaign/campaign-result.json
grep -a '"kind": "partition"' theseus-compose-campaign/campaign-result.json
grep -a '"kind": "storage_fault"' theseus-compose-campaign/campaign-result.json
if theseus compose explore --minimize theseus-compose-campaign --output stale-read-replay; then
    echo 'expected the minimized stale-read counterexample' >&2
    exit 1
fi
grep -a '"property": "consistent_read"' stale-read-replay/minimization.json
theseus compose replay stale-read-replay --output stale-read-rerun
grep -a 'counterexample: consistent_read' stale-read-rerun/services/api/result.json
theseus report --output campaign-report theseus-compose-campaign
grep -a 'Autonomous Compose campaign' campaign-report/index.html
echo 'PASS: Theseus found and reported the intentional stale-read timeline'
