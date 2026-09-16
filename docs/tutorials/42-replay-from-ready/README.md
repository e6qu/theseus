# Tutorial 42: Replay from a ready checkpoint

Boot an RNG-backed guest once. Retain its ready state, supply a serial reading,
and replay both standard random-device reads from that same state.

## Before you start

Use Linux with KVM and Docker. Run every host command from this tutorial directory.
Choose a published release containing ready-checkpoint replay:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

The guest reads a simulated UART TTY, as a Raspberry Pi application would read
`/dev/ttyS0` or `/dev/ttyAMA0`. The matching published kernel module supplies
seeded bytes for `/dev/urandom` and `/dev/random`; the Linux CRNG is not replayed.

## 1. Build the test inputs

```sh
sed -n '1,100p' init
sed -n '1,100p' theseus.toml
```

Keep `replay_start = "ready_checkpoint"`, virtual time, and the ready-gated UART
event. The guest must wait for input after printing `THES:M:42`.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Use `linux/amd64` on an amd64 host. Run the remaining commands inside this shell.

## 3. Prepare the guest

```sh
mkdir -p work/runtime work/guest/root/bin
cp /usr/local/bin/firecracker work/runtime/firecracker
cp /opt/theseus/vmlinux work/guest/vmlinux
cp /bin/busybox work/guest/root/bin/busybox
cp /opt/theseus/theseus_rng.ko work/guest/root/theseus_rng.ko
cp init work/guest/root/init
chmod +x work/guest/root/init
(cd work/guest/root && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
```

## 4. Run the test

```sh
theseus test --output work/replay theseus.toml
cat work/replay/serial.log
```

Expect `sensor reading: 21.5C`, then 16 bytes from each random device.
Theseus boots once into `work/replay/boot`, pauses at readiness, saves
`work/replay/checkpoint`, and restores that checkpoint for this first result.
`serial.log` contains only resumed output; `checkpoint/prelude.log` retains boot
output separately. Checks apply only to resumed output.

## 5. Inspect and replay

```sh
ls -lh work/replay/checkpoint
grep -A4 '"start"' work/replay/execution.json
theseus replay --output work/rerun work/replay
cmp work/replay/execution.json work/rerun/execution.json
cmp work/replay/serial.log work/rerun/serial.log
grep -A4 'replay_machine_execution' work/rerun/result.json
```

Expect a passed replay check. The version-3 plan locks the runtime, guest,
checkpoint metadata, VM state, RAM, and prelude. Replay verifies them before
resume and admits the exact retained execution suffix through guest exit.
The evidence identifies the checkpoint and inherited decision count: the
prefix was captured, not replayed from kernel boot.

Keep the checkpoint intact. Changed members or an incompatible CPU/runtime
fail; they never select fresh boot as a fallback. Kernel timers, arbitrary
instruction order, live network/storage devices, and cross-architecture restore
remain outside this workflow's guarantees. Checkpoints contain guest RAM and
may contain secrets.

## 6. Clean up (optional)

```sh
rm -rf work
exit
```
