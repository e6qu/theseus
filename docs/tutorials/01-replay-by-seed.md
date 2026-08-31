# Tutorial: Replay by seed

## Objective

Boot the same virtual machine twice with the same seed and prove that the
randomness the guest sees is byte-identical, then boot with a different
seed and prove it differs. When you finish, you will know that every run
in this project can be replayed exactly — and you will have seen it with
your own eyes.

## The problem

Tests that depend on randomness flake. A run that passes today fails
tomorrow because the operating system served different random bytes, and
you cannot rerun the failing case because those bytes are gone forever.
The fix is to make randomness a pure function of a *seed* — a number you
choose. Same seed, same bytes, every time.

This tutorial applies that idea to a whole virtual machine. The guest's
entropy device (`/dev/hwrng`, the file the Linux kernel uses to seed its
own random number generator) serves bytes from a seeded stream instead of
host randomness.

## Prerequisites

You need a Linux machine with KVM (the kernel virtual machine feature).
On Apple Silicon macOS, a privileged aarch64 Docker container provides it.
From the repository root, start it like this:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0 bash
```

Inside the container, install the build dependencies and build the
Firecracker fork once:

```sh
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev gcc cpio curl
cd /theseus/firecracker && cargo build -p firecracker
```

## Run the experiment

From the repository root inside the container:

```sh
sh /theseus/e2e/run.sh
```

The script does three things for you: it downloads a Linux guest kernel,
packs a tiny initramfs (a minimal root filesystem whose `/init` program
reads 64 bytes from `/dev/hwrng` and `/dev/urandom`, prints them as hex,
and powers off), and boots the guest three times — twice with seed 42 and
once with seed 1337.

You will see output like this:

```
hwrng (64 bytes): 28065689f706c281a35be8609b92dce6...
hwrng (64 bytes): 28065689f706c281a35be8609b92dce6...
hwrng (64 bytes): 70788b9d6210d1870cda0f02887c9e28...
PASS: hwrng deterministic per seed (identical across same-seed runs, differs across seeds)
note: urandom diverges on same-seed runs — guest kernel mixes timing-jitter entropy (known guest-internal leak)
```

Read the three hex lines: the first two (seed 42) are identical; the
third (seed 1337) is different. That is the whole claim, proven: the
entropy stream is a pure function of the seed.

The note at the end is an honest boundary: `/dev/urandom` — the kernel's
own random pool, which mixes in timing jitter generated inside the guest —
still differs between boots. A hypervisor can make everything the *host*
serves deterministic; what the guest kernel adds on top is outside that
boundary.

## What just happened

The guest booted with its entropy device configured with a seed. Every
byte it read came from a ChaCha stream (a keyed pseudorandom byte
generator) initialized from that seed. No host randomness was consulted,
so nothing could vary between runs.

## What you have now

Proof that runs replay exactly. Whenever anything goes wrong in a later
experiment, you can rerun it with the same seed and see the same thing —
the foundation for every test built on this engine.

## Further reading

- [determinism.md](../determinism.md) — every source of nondeterminism in
  a virtual machine and how the engine closes it
- [terminology.md](../terminology.md) — definitions of seed, replay, and
  related terms
