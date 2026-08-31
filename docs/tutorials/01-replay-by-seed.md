# Tutorial: Replay a failing test by seed

## Objective

Prove that a run in Theseus is perfectly replayable: boot the same guest
twice with the same seed and see byte-identical randomness, then boot with
a different seed and see it differ. When you finish, you will know that a
failing run is never lost — you rerun the seed and see the same thing.

## The problem

Your test fails once in a hundred runs. The failure depends on randomness
— an order of operations, a timeout value, a jitter — and when it fails
you cannot reproduce it because the randomness is gone. The fix is to
make randomness a pure function of a *seed*: a number you choose. Same
seed, same bytes, every time.

## Setup

You need a Linux machine with KVM (the kernel virtual machine feature).
On Apple Silicon macOS, a privileged aarch64 Docker container provides it.
From the repository root:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0 bash
```

Inside the container, install dependencies and build the fork once:

```sh
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev gcc cpio curl
cd /theseus/firecracker && cargo build -p firecracker
```

## Run it

From the repository root inside the container:

```sh
sh /theseus/e2e/run.sh
```

The script downloads a guest kernel, packs a minimal initramfs whose init
reads 64 bytes from the guest's entropy device and prints them, and boots
the guest three times: twice with seed 42, once with seed 1337. You will
see:

```
hwrng (64 bytes): 28065689f706c281a35be8609b92dce6...
hwrng (64 bytes): 28065689f706c281a35be8609b92dce6...
hwrng (64 bytes): 70788b9d6210d1870cda0f02887c9e28...
PASS: hwrng deterministic per seed (identical across same-seed runs, differs across seeds)
```

The first two lines are identical (seed 42); the third differs (seed
1337).

## What just happened

The guest's entropy device served bytes from a ChaCha stream initialized
from the seed — not from host randomness. Same seed, same stream, every
boot. The kernel's own random pool (`/dev/urandom`) still differs between
boots because the guest kernel adds timing jitter — randomness generated
inside the guest that no host can control. That is the boundary: what the
host serves replays; what the guest adds on top does not.

## What you have now

A failing run is a seed. When a test breaks, you keep the seed and rerun
the failure exactly — the basis for every experiment that follows.

## Further reading

- [determinism.md](../determinism.md) — every nondeterminism source and
  how the engine closes it
- [terminology.md](../terminology.md) — seed, replay, and related terms
