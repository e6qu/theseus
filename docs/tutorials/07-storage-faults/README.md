# Tutorial 7: Inject a storage fault

Write one sector to an in-memory `/dev/vda`, read it back, observe configured
corruption, and replay the same simulated-drive state.

Use Linux with KVM and Docker. Run every host command from this directory:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
docker run --rm -it --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining steps inside the container.

## 1. Inspect the storage contract

```sh
sed -n '1,200p' service/theseus.toml
sed -n '1,160p' service/init
```

The simulated drive never names or touches a host disk file.

## 2. Build the guest input

```sh
mkdir -p service/runtime service/guest/root/bin
cp /usr/local/bin/firecracker service/runtime/firecracker
cp /opt/theseus/vmlinux service/guest/vmlinux
cp /bin/busybox service/guest/root/bin/busybox
for applet in mkdir mount dd cmp poweroff; do
  ln -sf busybox "service/guest/root/bin/$applet"
done
cp service/init service/guest/root/init
chmod +x service/guest/root/init
(cd service/guest/root && find . -print | cpio -o -H newc --quiet | gzip > ../initramfs.cpio.gz)
```

## 3. Run and replay

```sh
theseus compose test
grep -a '^storage fault observed$' theseus-compose-replay/services/service/serial.log
theseus compose replay theseus-compose-replay --output topology-replay
grep -a 'simulated storage matches the original replay bundle' topology-replay/services/service/result.json
exit
```

Change one `[[storage]]` value, remove the generated bundles, and repeat to
observe a different locked contract.

## 4. Clean up (optional)

```sh
rm -rf service/runtime service/guest theseus-compose-replay topology-replay
```
