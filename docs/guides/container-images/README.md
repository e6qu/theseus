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

The published Linux runtime exposes the same adapter as `theseus-image`:

```sh
theseus-image flatten /tmp/image.tar --output guest/initramfs.cpio
```

It writes a bootable initramfs and prints the locked image command,
environment, and working directory as JSON. Point a normal Theseus manifest
at that `guest/initramfs.cpio`; the regular test, Compose, campaign,
minimization, and replay paths then lock it like any other guest artifact.
The lower-level Rust API remains `orchestrator::oci::flatten`.

Static and dynamically linked images work when their dependencies are inside
the image. Use the simulated network for deterministic networking.

See [the control channel](../../control-channel.md) and
[determinism](../../determinism.md).
