#!/bin/sh
# Tutorial 01: replay by seed. Boots a tiny guest three times (seed 42, 42,
# 1337) and proves the entropy stream is identical for equal seeds and
# different across seeds. Run inside the privileged Linux container.
set -e

DIR=$(dirname "$0")
FC=${FC:-/theseus/firecracker/build/cargo_target/debug/firecracker}
KERNEL=$DIR/vmlinux

[ -f "$KERNEL" ] || {
    echo ">> downloading the guest kernel"
    curl -sSL "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.9/aarch64/vmlinux-5.10.225" -o "$KERNEL"
}

echo ">> building the initramfs"
gcc -static -O2 -o /tmp/t01-init "$DIR/init.c"
rm -rf /tmp/t01-ir && mkdir -p /tmp/t01-ir && cp /tmp/t01-init /tmp/t01-ir/init
( cd /tmp/t01-ir && find . | cpio -o -H newc --quiet | gzip > /tmp/t01-ir.gz )

boot() {
    seed=$1
    log=$2
    sock=/tmp/t01-$seed.sock
    rm -f "$sock"
    "$FC" --api-sock "$sock" --no-seccomp > "$log" 2>&1 &
    fc_pid=$!
    for i in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done
    curl -s --unix-socket "$sock" -X PUT localhost/boot-source \
        -H 'Content-Type: application/json' \
        -d "{\"kernel_image_path\": \"$KERNEL\", \"initrd_path\": \"/tmp/t01-ir.gz\", \"boot_args\": \"console=ttyS0 reboot=k panic=-1\"}" >/dev/null
    curl -s --unix-socket "$sock" -X PUT localhost/machine-config \
        -H 'Content-Type: application/json' \
        -d '{"vcpu_count": 1, "mem_size_mib": 128}' >/dev/null
    curl -s --unix-socket "$sock" -X PUT localhost/entropy \
        -H 'Content-Type: application/json' \
        -d "{\"seed\": $seed}" >/dev/null
    curl -s --unix-socket "$sock" -X PUT localhost/actions \
        -H 'Content-Type: application/json' \
        -d '{"action_type": "InstanceStart"}' >/dev/null
    for i in $(seq 1 100); do kill -0 $fc_pid 2>/dev/null || break; sleep 0.2; done
    kill $fc_pid 2>/dev/null || true
    wait $fc_pid 2>/dev/null || true
}

echo ">> boot 1 (seed 42)"
boot 42 /tmp/t01-r1.log
echo ">> boot 2 (seed 42)"
boot 42 /tmp/t01-r2.log
echo ">> boot 3 (seed 1337)"
boot 1337 /tmp/t01-r3.log

h1=$(grep -a '^hwrng' /tmp/t01-r1.log)
h2=$(grep -a '^hwrng' /tmp/t01-r2.log)
h3=$(grep -a '^hwrng' /tmp/t01-r3.log)
echo "seed 42:   $h1"
echo "seed 42:   $h2"
echo "seed 1337: $h3"

if [ "$h1" = "$h2" ] && [ "$h1" != "$h3" ]; then
    echo "PASS: identical across same-seed boots, different across seeds"
else
    echo "FAIL"
    exit 1
fi
