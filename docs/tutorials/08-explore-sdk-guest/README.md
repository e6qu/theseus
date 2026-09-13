# Tutorial 8: Explore an SDK guest

Build a small SDK-instrumented aarch64 guest, explore up to seven timelines,
then replay the locked result. This is the second SDK example; tutorials 1,
2, and 4 use ordinary Linux device interfaces instead.

Run every host command from this directory. You need Rust, Docker, Linux on
arm64 with KVM, and a published release:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
```

## 1. Inspect and build the guest on the host

```sh
sed -n '1,240p' main.rs
sed -n '1,240p' theseus.toml
rm -rf vendor target guest.bin
mkdir vendor
curl -fsSL \
  "https://github.com/e6qu/theseus/releases/download/$THESEUS_TAG/theseus-sdk-0.1.0.crate" \
  | tar -xz -C vendor --strip-components=1
rustup target add aarch64-unknown-none
cargo build --release
objcopy -O binary \
  target/aarch64-unknown-none/release/theseus-explore-tutorial guest.bin
test -s guest.bin
```

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining steps inside the container.

## 3. Prepare and explore

```sh
mkdir -p runtime initramfs-root
cp /usr/local/bin/firecracker runtime/firecracker
(cd initramfs-root && find . -print | cpio -o -H newc --quiet > ../empty-initramfs.cpio)
theseus explore
grep -a '"status": "passed"' theseus-exploration/result.json
grep -a '"seed_path"' theseus-exploration/result.json
test -x theseus-exploration/artifacts/theseus-explorer
```

The result records each explored seed path. The bundle includes the exact
explorer binary selected for replay.

## 4. Replay

```sh
theseus explore --replay theseus-exploration --output exploration-replay
grep -a '"status": "passed"' exploration-replay/result.json
exit
```

## 5. Clean up (optional)

```sh
rm -rf vendor target guest.bin runtime initramfs-root empty-initramfs.cpio \
  theseus-exploration exploration-replay
```
