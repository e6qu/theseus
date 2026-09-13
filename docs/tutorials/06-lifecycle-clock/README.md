# Tutorial 6: Schedule a service fault

Pause and restart one service, then jump its virtual clock at deterministic
topology rounds. Replay the recorded lifecycle and clock state.

Use Linux with KVM and Docker. Run every host command from this directory:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
docker run --rm -it --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining steps inside the container.

## 1. Inspect the schedule

```sh
sed -n '1,240p' compose.yaml
sed -n '1,200p' service/theseus.toml
```

`at_round` counts topology rounds, not host time. A clock jump requires the
manifest's virtual-time configuration.

## 2. Build the guest input

```sh
mkdir -p service/runtime service/guest/root/bin
cp /usr/local/bin/firecracker service/runtime/firecracker
cp /opt/theseus/vmlinux service/guest/vmlinux
cp /bin/busybox service/guest/root/bin/busybox
for applet in mount sleep poweroff; do
  ln -sf busybox "service/guest/root/bin/$applet"
done
cp service/init service/guest/root/init
chmod +x service/guest/root/init
(cd service/guest/root && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
```

## 3. Run and inspect the recorded schedule

```sh
theseus compose test
grep -a '"kind": "clock_jump"' theseus-compose-replay/services/service/result.json
grep -a '^finished$' theseus-compose-replay/services/service/serial-1.log
```

## 4. Replay it

```sh
theseus compose replay theseus-compose-replay --output topology-replay
grep -a 'applied faults match the original replay bundle' topology-replay/services/service/result.json
grep -a 'virtual clock state matches the original replay bundle' topology-replay/services/service/result.json
exit
```

This proves equality of the retained end-of-run evidence. It does not prove
instruction-level equality for counter reads inside one virtual-time quantum.

## 5. Clean up (optional)

```sh
rm -rf service/runtime service/guest theseus-compose-replay topology-replay
```
