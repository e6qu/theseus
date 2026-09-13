# Tutorial 4: Replay a serial reading

Send one sensor reading to a Linux guest through its UART. The guest reads
`/dev/ttyS0` and prints the value back. No SDK is involved.

Run every host command from this directory. You need Linux, KVM, Docker, and a
published Theseus runtime image:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
export READING=21.5C
```

## 1. Inspect the guest and harness

```sh
sed -n '1,160p' init
sed -n '1,260p' run.sh
```

`init` contains the actual guest read. `run.sh` is the low-level Firecracker
harness: it creates a FIFO for UART input, configures serial capture through
the Firecracker API, and boots the guest.

## 2. Send the reading

```sh
docker run --rm --privileged --platform linux/arm64 \
  -e READING -v "$PWD":/tutorial -w /tutorial \
  "$THESEUS_IMAGE" sh ./run.sh
```

The final lines must include:

```text
sensor reading: 21.5C
PASS: guest read deterministic UART input from ttyS0
```

Run the same command again to send the same bytes. Change `READING` to select
different deterministic input.

On a Raspberry Pi, a UART is exposed through the Linux TTY interface too,
commonly as `/dev/ttyAMA0` or `/dev/ttyS0`. The physical Pi has a real UART;
this tutorial uses Firecracker's simulated UART. In both cases an application
opens a TTY device and reads bytes, but Theseus records and injects those bytes
instead of reading an uncontrolled physical sensor.

## 3. Inspect the retained serial output

```sh
grep -a '^sensor reading:' work/serial-out.log
```

## 4. Clean up (optional)

```sh
rm -rf work
```
