# Tutorial: Control the random

## Objective

Force a guest's random number generator to return values you chose —
`1, 2, 3, ...` — before it falls back to the seeded stream. When you
finish, you will be able to drive any code path that depends on
randomness, deterministically.

## The problem

Your code rolls dice: a retry count, a backoff multiplier, a timeout, a
random node to ping. You want to test the "rolls a 3" path. With real
randomness you can't; with a seeded stream you get determinism but no
control over the actual values. Scripting the values closes that gap:
the entropy device serves bytes you wrote, verbatim, before the seeded
stream continues.

## Prerequisites

You need a Linux machine with KVM. On Apple Silicon macOS, a privileged
aarch64 Docker container provides it. From the repository root:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0-bookworm bash
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev gcc cpio curl
cd /theseus/firecracker && cargo build -p firecracker
```

## The configuration

The entropy device accepts a `script` — bytes served verbatim before the
seeded stream:

```json
{ "seed": 42, "script": [1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0] }
```

A guest reading little-endian `u64`s from `/dev/hwrng` (or from the
kernel's `getrandom`) sees exactly 1, then 2, then 3, then 4; after the
script is exhausted, the seeded stream continues from the same ChaCha
state as always. Replays and
branch children see the same script followed by the same stream — the
script's remaining bytes are part of the snapshot, so branching keeps the
schedule intact.

## Run it

Boot the tutorial guest with a scripted entropy device:

```sh
sh /theseus/docs/tutorials/02-control-the-random/run.sh
```

You will see:

```
random() = 1 2 3 4
PASS: random() returned 1, 2, 3, 4 — the values we scripted
```

The script supplies those four little-endian `u64` values verbatim. The
underlying device test, `cargo test -p vmm --lib
devices::virtio::rng::device::tests::test_script_served_before_stream`,
also verifies that the seeded stream resumes once a script is consumed.

## What you have now

A way to make "random" choices in your system deterministic *and* chosen:
force the retry count to 3, force the backoff to its maximum, force the
leader election to pick node B. Combined with the seeded stream, you get
chosen values first and reproducible randomness after — per run, per
branch, per replay.

## Further reading

- [determinism.md](../../determinism.md) — the full determinism model
- [terminology.md](../../terminology.md) — seed and replay definitions
