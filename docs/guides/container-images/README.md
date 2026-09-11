# Integration guide: Run a container image

Use this path when your system already ships as a container image. Theseus
flattens the image into an initramfs, adds its pivot as `/init`, then boots it
under the same service API. Your image does not need a Theseus guest driver.

## 1. Run a working image

Start with [tutorial 14](../../tutorials/14-container-image/). It uses only a
published Theseus runtime image and the files in its own directory.

## 2. Prepare your image

Export it in Docker's image-tar format:

```sh
docker save myimage > guest/service.tar
```

The published Linux runtime exposes the adapter as `theseus-image`. Add the
image and adapter to the same test directory as your manifest:

```toml
[runtime]
firecracker = "runtime/firecracker"
image_adapter = "runtime/theseus-image"

[guest]
kernel = "guest/vmlinux"
image = "guest/service.tar"
```

`theseus test` writes the bootable initramfs in its run directory and locks
both source artifacts in the replay bundle. It preserves the image command,
environment, and working directory. The regular test, Compose, and replay
paths then use that locked image; the lower-level Rust API remains
`orchestrator::oci::flatten`.

Static and dynamically linked images work when their dependencies are inside
the image. Use the simulated network for deterministic networking.

See [the control channel](../../control-channel.md) and
[determinism](../../determinism.md).
