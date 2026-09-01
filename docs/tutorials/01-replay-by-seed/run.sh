#!/bin/sh
# Run this file from this directory, inside a Theseus runtime image.
set -eu

fc=${FC:-/usr/local/bin/firecracker}
kernel=${THESEUS_KERNEL:-/opt/theseus/vmlinux}
module=${THESEUS_RNG_MODULE:-/opt/theseus/theseus_rng.ko}
work=$(mktemp -d /tmp/theseus-replay.XXXXXX)
trap 'rm -rf "$work"' EXIT

[ -x "$fc" ] && [ -f "$kernel" ] && [ -f "$module" ] || {
    echo 'Run this tutorial in the published arm64 Theseus runtime image.' >&2
    exit 1
}

mkdir -p "$work/root/bin"
cp /bin/busybox "$work/root/bin/busybox"
cp init "$work/root/init"
cp "$module" "$work/root/theseus_rng.ko"
chmod +x "$work/root/init"
(cd "$work/root" && find . -print | cpio -o -H newc --quiet | gzip > "$work/initramfs.cpio.gz")

boot() {
    seed=$1
    log=$2
    sock="$work/firecracker-$seed.sock"
    rm -f "$sock"
    "$fc" --api-sock "$sock" --no-seccomp >"$log" 2>&1 &
    pid=$!
    for _ in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done
    curl -fsS --unix-socket "$sock" -X PUT localhost/boot-source \
        -H 'Content-Type: application/json' \
        -d "{\"kernel_image_path\":\"$kernel\",\"initrd_path\":\"$work/initramfs.cpio.gz\",\"boot_args\":\"console=ttyS0 reboot=k panic=-1\"}" >/dev/null
    curl -fsS --unix-socket "$sock" -X PUT localhost/machine-config \
        -H 'Content-Type: application/json' -d '{"vcpu_count":1,"mem_size_mib":128}' >/dev/null
    curl -fsS --unix-socket "$sock" -X PUT localhost/entropy \
        -H 'Content-Type: application/json' -d "{\"seed\":$seed}" >/dev/null
    curl -fsS --unix-socket "$sock" -X PUT localhost/actions \
        -H 'Content-Type: application/json' -d '{"action_type":"InstanceStart"}' >/dev/null
    for _ in $(seq 1 100); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

boot 42 "$work/first.log"
boot 42 "$work/second.log"
boot 1337 "$work/third.log"

first=$(grep -aE '^(urandom|random):' "$work/first.log" | tr '\n' ' ')
second=$(grep -aE '^(urandom|random):' "$work/second.log" | tr '\n' ' ')
third=$(grep -aE '^(urandom|random):' "$work/third.log" | tr '\n' ' ')
printf 'seed 42:   %s\n' "$first"
printf 'seed 42:   %s\n' "$second"
printf 'seed 1337: %s\n' "$third"

[ "$first" = "$second" ] && [ "$first" != "$third" ]
echo 'PASS: both standard random devices replay by seed'
