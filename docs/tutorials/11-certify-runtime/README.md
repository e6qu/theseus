# Tutorial 11: Record a runtime repeatability witness

Boot one fixed topology, retain its ready state, and restore it twice on a
native KVM host. Save the comparison as a machine-readable certificate.
The witness applies only to the recorded checkpoint, plan,
architecture, kernel, modules, and runtime artifacts.

Use Linux on amd64 or arm64 with KVM and Docker. Run every host command from
this directory and choose the matching published image suffix:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_ARCH=arm64
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-${THESEUS_ARCH}
docker run --rm -it --privileged \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining steps inside the container.

## 1. Inspect and build the guest input

```sh
sed -n '1,220p' compose.yaml
sed -n '1,200p' service/theseus.toml
mkdir -p service/runtime service/guest/root/bin
cp /usr/local/bin/firecracker service/runtime/firecracker
cp /opt/theseus/vmlinux service/guest/vmlinux
cp /bin/busybox service/guest/root/bin/busybox
for applet in mkdir mount reboot; do
  ln -sf busybox "service/guest/root/bin/$applet"
done
cp service/init service/guest/root/init
chmod +x service/guest/root/init
(cd service/guest/root && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
```

The init script prints `THES:M:42`, then waits for `finish` on its serial TTY.
Inspect `service/init` before building it. `x-theseus.replay_start` asks the
runner to capture every service and the simulated network at that ready boundary.
Both executions restore this checkpoint before sending the recorded UART input.
The manifest gives Linux enough deterministic VM-exit rounds to finish booting;
the limit is a reproducible work budget, not a wall-clock timeout.

## 2. Run both executions

```sh
theseus compose plan > plan.json
theseus-topology certify --plan plan.json --output certificate
```

Certification rejects missing KVM, missing virtual time, and unsupported
host-backed I/O. A successful command performed two executions.
It did not replay kernel boot: the complete boot trace is retained ancestry.

## 3. Inspect the witness

```sh
grep -F '"status": "passed"' certificate/certificate.json
grep -F '"executions": 2' certificate/certificate.json
grep -F '"format": "theseus-runtime-certificate-v5"' certificate/certificate.json
cat certificate/first/checkpoint/starting-state/metadata.json
grep -A4 '"execution_start"' certificate/replay/services/service/result.json
grep -F '"name": "replay_entropy"' certificate/replay/services/service/result.json
exit
```

This establishes equality of the recorded serial, entropy, storage, network,
clock and active execution evidence for this checkpoint and plan. The locked
metadata binds VM state, RAM, and `context.bin` (NIC/switch queues, seeded link
state, UART transcript, scheduler cursors, control state, and inherited traces).
Keep the entire `certificate` directory when copying the witness.

This example does not certify fresh-boot restart, kernel boot repeatability,
physical-device input, or instruction-by-instruction execution. A scheduled
restart introduces another boot and can still diverge. A failed active replay
retains its first error and partial stream as `services/<name>/execution-error.json`,
including when dependency startup never completes.

## 4. Clean up (optional)

```sh
rm -rf service/runtime service/guest plan.json certificate
```
