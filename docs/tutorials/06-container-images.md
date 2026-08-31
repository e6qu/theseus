# Tutorial: Run a container image as a virtual machine

## Objective

Take a container image — the same artifact your CI builds and your
production runs — and boot it as a virtual machine under Theseus, with the
control channel already wired in. When you finish, you will have flattened
an image into a bootable filesystem, booted it, and read your payload's
output on the serial console. You will not have written a Dockerfile,
installed a guest driver, or modified the image.

## The problem

Your tests should exercise the same artifact you ship. But a container
image is not bootable: it is a pile of filesystem layers plus some
metadata, with no kernel, no init, and no devices. Theseus closes that
gap by flattening the image into an initramfs (a minimal in-memory root
filesystem the kernel unpacks at boot) and injecting a tiny static
`/init` — the *pivot* — that mounts the essentials, opens the control
channel over the serial console, and then runs your image's entrypoint.
Your image needs no Theseus code of its own.

Key terms: an *image* in this tutorial is a `docker save`-format tar
(`manifest.json`, a config JSON, and layer tars). *Flattening* means
applying the layers in order to produce one filesystem, honoring
whiteouts (the marker files that delete lower-layer files). The *pivot* is
the static binary that runs as PID 1 and hands off to your entrypoint.

## Prerequisites

You need a Linux machine with KVM (the kernel virtual machine feature).
On Apple Silicon macOS, a privileged aarch64 Docker container provides it.
From the repository root, start it like this:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0 bash
```

Inside the container, install dependencies and build the fork once:

```sh
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev gcc curl
cd /theseus/firecracker && cargo build -p firecracker
```

## Step 1 — Flatten an image

The flattener is `flatten` in `orchestrator/src/oci.rs`. Its complete
interface:

```rust
let (initramfs_bytes, spec) = flatten(&image_tar_bytes)?;
// spec.argv / spec.env / spec.workdir come from the image config
```

Write `initramfs_bytes` to a file; it is a bootable initramfs.

Three input formats land in the same place:

- **An image tar** (`docker save myimage > image.tar`) — pass it directly.
- **A Dockerfile** — build and export it first:
  `docker build -t myimage . && docker save myimage > image.tar`.
- **A registry image** — pull and export it first:
  `docker pull registry.example.com/myimage:latest && docker save ... > image.tar`.

## Step 2 — Watch the full path run

The repository has an end-to-end test that builds a tiny image in memory
(a static payload binary that prints `CONTAINER-PAYLOAD-OK`), flattens
it, boots it with a stock kernel, and reads the serial console. Run it:

```sh
cd /theseus/orchestrator
cargo test --lib oci::tests::test_boot_container_image
```

You will see it pass. The serial log shows the pivot's boot marker and
then the payload:

```
THES:M:42
CONTAINER-PAYLOAD-OK
```

The pivot reported the boot marker over the serial control channel,
mounted `/dev`, `/proc`, and `/sys`, read the init spec written by the
flattener, and executed your entrypoint. The payload needed no Theseus
code.

## Step 3 — Use your own image

To boot a real image, flatten it as in step 1 and boot it with the stock
CI kernel and your initramfs:

```sh
docker save myimage > /tmp/image.tar    # or pull+save from a registry
```

Then boot via the API (the `boot-source` call takes `kernel_image_path`
and `initrd_path`):

```sh
curl --unix-socket /tmp/fc.sock -X PUT localhost/boot-source \
  -H 'Content-Type: application/json' \
  -d '{"kernel_image_path": "/theseus/e2e/vmlinux", "initrd_path": "/tmp/initramfs.cpio", "boot_args": "console=ttyS0 reboot=k panic=-1"}'
```

What you can and cannot do today:

- Static binaries and self-contained runtimes work. Images that need a
  dynamic loader work too — the loader ships inside the image by
  definition.
- Networking inside the VM uses the simulated backend (deterministic,
  partitionable) — configure it with the `sim` network config.
- Guest kernel timing jitter is outside the replay boundary, as in every
  boot.

## What you have now

A path from "the artifact CI already builds" to "a seeded, replayable
virtual machine that talks the Theseus protocol" — without rebuilding or
modifying the image. Every other tutorial's machinery (branching, faults,
virtual time) applies to this VM unchanged.

## Further reading

- [control-channel.md](../control-channel.md) — the serial transport the
  pivot uses
- [determinism.md](../determinism.md) — what replays and what does not
- [terminology.md](../terminology.md) — flattening, pivot, and related
  terms
