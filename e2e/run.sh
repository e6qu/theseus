#!/bin/sh
# Theseus e2e: boot a microVM with seeded entropy, three times.
# Twice with seed 42 (must be identical), once with seed 1337 (must differ).
# Runs inside the privileged aarch64 Linux container (needs /dev/kvm).
set -e

E2E=/theseus/e2e
FC_DIR=${FC_DIR:-/theseus/firecracker}
FC=$FC_DIR/build/cargo_target/debug/firecracker
KERNEL=$E2E/vmlinux
INITRD=$E2E/initramfs.cpio.gz
S3=https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.9/aarch64

# 1. Guest kernel.
if [ ! -f "$KERNEL" ]; then
    echo ">> downloading vmlinux-5.10.225"
    curl -sSL "$S3/vmlinux-5.10.225" -o "$KERNEL"
fi

# 2. initramfs with our init.
echo ">> building initramfs"
gcc -static -O2 -o /tmp/init "$E2E/init.c"
mkdir -p /tmp/ir
cp /tmp/init /tmp/ir/init
( cd /tmp/ir && find . | cpio -o -H newc --quiet | gzip > "$INITRD" )
# 3. Boot function: seed in, serial log out.
boot() {
    seed=$1
    log=$2
    sock=/tmp/fc-$seed.sock
    rm -f "$sock"
    "$FC" --api-sock "$sock" --no-seccomp > "$log" 2>&1 &
    fc_pid=$!
    for i in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done

    curl -s --unix-socket "$sock" -X PUT localhost/boot-source \
        -H 'Content-Type: application/json' \
        -d "{\"kernel_image_path\": \"$KERNEL\", \"initrd_path\": \"$INITRD\", \"boot_args\": \"console=ttyS0 reboot=k panic=-1\"}"
    curl -s --unix-socket "$sock" -X PUT localhost/machine-config \
        -H 'Content-Type: application/json' \
        -d '{"vcpu_count": 1, "mem_size_mib": 128}'
    curl -s --unix-socket "$sock" -X PUT localhost/entropy \
        -H 'Content-Type: application/json' \
        -d "{\"seed\": $seed}"
    curl -s --unix-socket "$sock" -X PUT localhost/actions \
        -H 'Content-Type: application/json' \
        -d '{"action_type": "InstanceStart"}'

    # Wait for the guest to power off (firecracker exits).
    for i in $(seq 1 100); do kill -0 $fc_pid 2>/dev/null || break; sleep 0.2; done
    kill $fc_pid 2>/dev/null || true
    wait $fc_pid 2>/dev/null || true
}

echo ">> boot 1 (seed 42)"
boot 42 /tmp/run1.log
echo ">> boot 2 (seed 42)"
boot 42 /tmp/run2.log
echo ">> boot 3 (seed 1337)"
boot 1337 /tmp/run3.log

# 4. Compare.
extract() { grep -E '^(hwrng|urandom)' "$1"; }
extract /tmp/run1.log > /tmp/e1
extract /tmp/run2.log > /tmp/e2
extract /tmp/run3.log > /tmp/e3

echo "== run 1 =="; cat /tmp/e1
echo "== run 2 =="; cat /tmp/e2
echo "== run 3 =="; cat /tmp/e3

# The contract we control: the virtio-rng device must be seed-deterministic.
h1=$(grep '^hwrng' /tmp/e1)
h2=$(grep '^hwrng' /tmp/e2)
h3=$(grep '^hwrng' /tmp/e3)
if [ "$h1" = "$h2" ] && [ "$h1" != "$h3" ]; then
    echo "PASS: hwrng deterministic per seed (identical across same-seed runs, differs across seeds)"
else
    echo "FAIL: hwrng not seed-deterministic"
    exit 1
fi

# Informational: the kernel CSPRNG also mixes timing-jitter entropy, which is
# a guest-internal leak we cannot close from the hypervisor. Report it.
if cmp -s /tmp/e1 /tmp/e2; then
    echo "note: urandom also deterministic (kernel mixed only deterministic sources)"
else
    echo "note: urandom diverges on same-seed runs — guest kernel mixes timing-jitter entropy (known guest-internal leak)"
fi

# 5. Control channel: boot the bare-metal guest; it reads the magic register
# over MMIO, prints it to serial, and issues setup-complete + a log marker.
GUEST=$FC_DIR/src/vmm/src/test_utils/mock_resources/theseus_guest.bin
if [ -f "$GUEST" ]; then
    echo ">> boot 4 (control channel guest)"
    sock=/tmp/fc-door.sock
    rm -f "$sock"
    "$FC" --api-sock "$sock" --no-seccomp > /tmp/door.log 2>&1 &
    fc_pid=$!
    for i in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done
    curl -s --unix-socket "$sock" -X PUT localhost/boot-source \
        -H 'Content-Type: application/json' \
        -d "{\"kernel_image_path\": \"$GUEST\", \"boot_args\": \"console=ttyS0 reboot=k panic=-1 pci=off\"}"
    curl -s --unix-socket "$sock" -X PUT localhost/machine-config \
        -H 'Content-Type: application/json' \
        -d '{"vcpu_count": 1, "mem_size_mib": 128}'
    curl -s --unix-socket "$sock" -X PUT localhost/actions \
        -H 'Content-Type: application/json' \
        -d '{"action_type": "InstanceStart"}'
    sleep 2
    kill $fc_pid 2>/dev/null || true
    wait $fc_pid 2>/dev/null || true

    if grep -aq 'guest: magic=THES' /tmp/door.log; then
        echo "PASS: control channel (guest read magic over MMIO; guest then waits in its event loop)"
    else
        echo "FAIL: control channel guest output missing:"
        grep -aE 'guest:' /tmp/door.log || echo "(no guest output)"
        exit 1
    fi
else
    echo "note: $GUEST not built (run make_theseus_guest.sh) — skipping door test"
fi

# 6. Linux-guest SDK transport: the serial-console control channel. The agent
# (static musl binary) is /init: dumps entropy, then echoes THES:E events as
# THES:M markers over ttyS0.
AGENT=/theseus/e2e/agent/target/aarch64-unknown-linux-musl/release/theseus-agent
echo ">> building theseus-agent"
rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1 || true
( cd /theseus/e2e/agent && cargo build --release 2>&1 | tail -2 )

if [ -x "$AGENT" ]; then
    rm -rf /tmp/ir2 && mkdir -p /tmp/ir2
    cp "$AGENT" /tmp/ir2/init
    ( cd /tmp/ir2 && find . | cpio -o -H newc --quiet | gzip > /tmp/initramfs-agent.cpio.gz )

    echo ">> boot 5 (serial control channel, seed 42)"
    sock=/tmp/fc-serial.sock
    fifo=/tmp/fc-stdin.fifo
    rm -f "$sock" "$fifo"
    mkfifo "$fifo"
    # Hold the write end open for the VM's whole life; input sent before the
    # guest's UART driver initializes is dropped, so we handshake: wait for
    # the agent's boot marker, then send the event round.
    "$FC" --api-sock "$sock" --no-seccomp < "$fifo" > /tmp/serial.log 2>&1 &
    fc_pid=$!
    exec 9>"$fifo"
    for i in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.1; done
    curl -s --unix-socket "$sock" -X PUT localhost/boot-source \
        -H 'Content-Type: application/json' \
        -d "{\"kernel_image_path\": \"$KERNEL\", \"initrd_path\": \"/tmp/initramfs-agent.cpio.gz\", \"boot_args\": \"console=ttyS0 reboot=k panic=-1\"}"
    curl -s --unix-socket "$sock" -X PUT localhost/machine-config \
        -H 'Content-Type: application/json' \
        -d '{"vcpu_count": 1, "mem_size_mib": 256}'
    curl -s --unix-socket "$sock" -X PUT localhost/entropy \
        -H 'Content-Type: application/json' \
        -d '{"seed": 42}'
    curl -s --unix-socket "$sock" -X PUT localhost/actions \
        -H 'Content-Type: application/json' \
        -d '{"action_type": "InstanceStart"}'
    for i in $(seq 1 100); do grep -aq 'THES:M:42' /tmp/serial.log && break; sleep 0.2; done
    printf 'THES:E:90\nTHES:E:00\n' >&9
    for i in $(seq 1 150); do grep -aq 'THES:M:ff' /tmp/serial.log && break; kill -0 $fc_pid 2>/dev/null || break; sleep 0.2; done
    exec 9>&-
    kill $fc_pid 2>/dev/null || true
    wait $fc_pid 2>/dev/null || true

    if grep -aq 'THES:M:42' /tmp/serial.log && grep -aq 'THES:M:90' /tmp/serial.log \
        && grep -aq 'THES:M:ff' /tmp/serial.log; then
        echo "PASS: serial control channel (Linux guest agent echoed event 0x90, done marker seen)"
    else
        echo "FAIL: serial channel markers missing:"
        grep -a 'THES:' /tmp/serial.log || echo "(none)"
        exit 1
    fi
fi
