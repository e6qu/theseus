# Tutorial 4: Replay a serial reading

Read `21.5C` from `/dev/ttyS0`, retain the UART input and device decisions,
and replay the exact stream. Use BusyBox shell commands; no SDK is needed.

Use Linux with KVM and Docker. Run every host command from this tutorial directory.
Choose a published release containing machine-stream capture:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

On a Raspberry Pi, an application reads its UART through a TTY such as
`/dev/ttyAMA0` or `/dev/ttyS0`. Here it reads Firecracker's simulated UART,
not a physical sensor. Theseus supplies the bytes declared in `theseus.toml`.

## 1. Inspect the guest and input

```sh
sed -n '1,80p' init
sed -n '1,100p' theseus.toml
```

The guest disables input echo, announces readiness, reads one line, prints it,
and requests reboot. The manifest supplies `21.5C` followed by a newline.

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
cp init work/guest/root/init
chmod +x work/guest/root/init
(cd work/guest/root && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
```

## 4. Run and inspect

```sh
theseus test --output work/replay theseus.toml
grep -a '^sensor reading: 21.5C$' work/replay/serial.log
grep -o 'host:serial_input:[^" ]*' work/replay/execution.json
```

Expect `sensor reading: 21.5C` and `host:serial_input:6:32312e35430a`.
`execution.json` retains the complete ordered trace and its digests, not just
the printed reading.

## 5. Replay the exact input and device stream

```sh
theseus replay --output work/rerun work/replay
cmp work/replay/execution.json work/rerun/execution.json
grep -A4 'replay_machine_execution' work/rerun/result.json
```

Expect a passed replay check. Replay uses the copied runtime, kernel, and guest
from `work/replay`; it does not read the original manifest or guest files.
A changed stream fails and leaves diagnostic evidence in `work/rerun`.
Kernel timers and guest instruction ordering remain outside Theseus's control.

## 6. Clean up (optional)

```sh
rm -rf work
exit
```
