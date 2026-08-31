# Integration guide: Run a container image

Use this path when your system already ships as a container image. Theseus
flattens the image into an initramfs, adds its pivot as `/init`, then boots it
under the same service API. Your image does not need a Theseus guest driver.

## 1. Run the repository proof

Use the Linux+KVM container from tutorial 1, then run:

```sh
cd /theseus/orchestrator
cargo test --lib oci::tests::test_boot_container_image
```

The test builds a small image, flattens it, boots it, and checks the serial
output:

```text
THES:M:42
CONTAINER-PAYLOAD-OK
```

## 2. Prepare your image

Export it in Docker's image-tar format:

```sh
docker save myimage > /tmp/image.tar
```

The current Rust API is `orchestrator::oci::flatten`. It returns bootable
initramfs bytes and the image's command, environment, and working directory.
Use the resulting initramfs in the normal `PUT /boot-source` request.

Static and dynamically linked images work when their dependencies are inside
the image. Use the simulated network for deterministic networking.

See [the control channel](../../control-channel.md) and
[determinism](../../determinism.md).
