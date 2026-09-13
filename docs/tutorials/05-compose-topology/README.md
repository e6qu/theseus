# Tutorial 5: Run two connected services

Boot two tiny Linux services on a deterministic Compose network. The `api`
guest receives `ping` through its UART and reaches `worker` over the simulated
backplane.

Use Linux with KVM and Docker. Run every host command from this directory:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
docker run --rm -it --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining steps inside the container.

## 1. Inspect the topology

```sh
sed -n '1,240p' compose.yaml
sed -n '1,200p' api/theseus.toml
sed -n '1,200p' worker/theseus.toml
```

The manifests use local guest files only. The network adds one round of
latency, up to one seeded jitter round, and duplicates accepted frames.

## 2. Build both initramfs images

```sh
for service in api worker; do
  mkdir -p "$service/runtime" "$service/guest/root/bin"
  cp /usr/local/bin/firecracker "$service/runtime/firecracker"
  cp /opt/theseus/vmlinux "$service/guest/vmlinux"
  cp /bin/busybox "$service/guest/root/bin/busybox"
  for applet in mount ip sleep ping poweroff; do
    ln -sf busybox "$service/guest/root/bin/$applet"
  done
  cp "$service/init" "$service/guest/root/init"
  chmod +x "$service/guest/root/init"
  (cd "$service/guest/root" && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
done
```

## 3. Run and inspect the topology

```sh
theseus compose test
grep -a '^ping passed$' theseus-compose-replay/services/api/serial.log
grep -a '^serial command accepted$' theseus-compose-replay/services/api/serial.log
grep -a '"jitter_rounds": 1' theseus-compose-replay/replay-plan.json
```

Those lines show that the UART command ran, the service reached its peer, and
the locked plan retained the network configuration.

## 4. Replay the locked bundle

```sh
theseus compose replay theseus-compose-replay --output topology-replay
grep -a 'serial logs match the original replay bundle' topology-replay/services/api/result.json
grep -a 'simulated network traffic matches the original replay bundle' topology-replay/services/api/result.json
exit
```

## 5. Clean up (optional)

```sh
rm -rf api/runtime api/guest worker/runtime worker/guest \
  theseus-compose-replay topology-replay
```
