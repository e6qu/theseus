#!/bin/sh
# Tutorial 02: control the random. Boots a guest whose entropy device is
# configured with a script of bytes — [1, 2, 3, 4] as little-endian u64s —
# served verbatim before the seeded stream. The guest prints four u64s from
# its randomness: 1, 2, 3, then a value from the seeded stream.
set -e

DIR=$(dirname "$0")
FC=${FC:-/theseus/firecracker/build/cargo_target/debug/firecracker}
KERNEL=$DIR/vmlinux

[ -f "$KERNEL" ] || {
    echo ">> downloading the guest kernel"
    curl -sSL "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.9/aarch64/vmlinux-5.10.225" -o "$KERNEL"
}

echo ">> building the initramfs"
gcc -static -O2 -o /tmp/t02-init "$DIR/init.c"
rm -rf /tmp/t02-ir && mkdir -p /tmp/t02-ir && cp /tmp/t02-init /tmp/t02-ir/init
( cd /tmp/t02-ir && find . | cpio -o -H newc --quiet | gzip > /tmp/t02-ir.gz )

sock=/tmp/t02.sock
rm -f "$sock"
"$FC" --api-sock "$sock" --no-seccomp > /tmp/t02.log 2>&1 &
fc_pid=$!
for i in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done

curl -s --unix-socket "$sock" -X PUT localhost/boot-source \
    -H 'Content-Type: application/json' \
    -d "{\"kernel_image_path\": \"$KERNEL\", \"initrd_path\": \"/tmp/t02-ir.gz\", \"boot_args\": \"console=ttyS0 reboot=k panic=-1\"}" >/dev/null
curl -s --unix-socket "$sock" -X PUT localhost/machine-config \
    -H 'Content-Type: application/json' \
    -d '{"vcpu_count": 1, "mem_size_mib": 128}' >/dev/null

# The script: [1, 2, 3, 4] as little-endian u64s, repeated so it outlasts
# the kernel's boot-time pool seeding. The guest kernel drains exactly 16
# bytes from the entropy device while seeding its pool (deterministic on
# this kernel), so the script starts with 16 throwaway bytes and the guest's
# first read lands on our values.
script=$( { yes '0,' | head -16; yes '1,0,0,0,0,0,0,0,2,0,0,0,0,0,0,0,3,0,0,0,0,0,0,0,4,0,0,0,0,0,0,0,' | head -128; } | tr -d '\n' | sed 's/,$//')
curl -s --unix-socket "$sock" -X PUT localhost/entropy \
    -H 'Content-Type: application/json' \
    -d "{\"seed\": 42, \"script\": [$script]}" >/dev/null

curl -s --unix-socket "$sock" -X PUT localhost/actions \
    -H 'Content-Type: application/json' \
    -d '{"action_type": "InstanceStart"}' >/dev/null

for i in $(seq 1 100); do kill -0 $fc_pid 2>/dev/null || break; sleep 0.2; done
kill $fc_pid 2>/dev/null || true
wait $fc_pid 2>/dev/null || true

line=$(grep -a '^random() =' /tmp/t02.log)
echo "$line"

case "$line" in
    "random() = 1 2 3 4") echo "PASS: random() returned 1, 2, 3, 4 — the values we scripted" ;;
    *) echo "FAIL"; exit 1 ;;
esac
