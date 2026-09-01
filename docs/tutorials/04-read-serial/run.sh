#!/bin/sh
# Run this file from this directory, inside a Theseus runtime image.
set -eu

fc=${FC:-/usr/local/bin/firecracker}
kernel=${THESEUS_KERNEL:-/opt/theseus/vmlinux}
reading=${READING:-21.5C}
work=$(mktemp -d /tmp/theseus-serial.XXXXXX)
trap 'rm -rf "$work"' EXIT

[ -x "$fc" ] && [ -f "$kernel" ] || {
    echo 'Run this tutorial in a published Theseus runtime image.' >&2
    exit 1
}

mkdir -p "$work/root/bin"
cp /bin/busybox "$work/root/bin/busybox"
cp init "$work/root/init"
chmod +x "$work/root/init"
(cd "$work/root" && find . -print | cpio -o -H newc --quiet | gzip > "$work/initramfs.cpio.gz")

sock="$work/firecracker.sock"
serial_in="$work/serial-in"
serial_out="$work/serial-out.log"
mkfifo "$serial_in"
"$fc" --api-sock "$sock" --no-seccomp <"$serial_in" >"$work/firecracker.log" 2>&1 &
pid=$!
# Opening the writer lets Firecracker finish opening its stdin, but keeps the
# UART open until the guest has read the sample.
exec 3>"$serial_in"
trap 'exec 3>&-; kill "$pid" 2>/dev/null || true; rm -rf "$work"' EXIT

for _ in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done
curl -fsS --unix-socket "$sock" -X PUT localhost/boot-source \
    -H 'Content-Type: application/json' \
    -d "{\"kernel_image_path\":\"$kernel\",\"initrd_path\":\"$work/initramfs.cpio.gz\",\"boot_args\":\"console=ttyS0 reboot=k panic=-1\"}" >/dev/null
curl -fsS --unix-socket "$sock" -X PUT localhost/machine-config \
    -H 'Content-Type: application/json' -d '{"vcpu_count":1,"mem_size_mib":128}' >/dev/null
curl -fsS --unix-socket "$sock" -X PUT localhost/serial \
    -H 'Content-Type: application/json' -d "{\"serial_out_path\":\"$serial_out\"}" >/dev/null
curl -fsS --unix-socket "$sock" -X PUT localhost/actions \
    -H 'Content-Type: application/json' -d '{"action_type":"InstanceStart"}' >/dev/null

printf '%s\n' "$reading" >&3
for _ in $(seq 1 100); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
exec 3>&-
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true

grep -a "^sensor reading: $reading$" "$serial_out"
echo 'PASS: guest read deterministic UART input from ttyS0'
