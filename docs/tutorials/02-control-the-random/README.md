# Tutorial 2: Choose the random stream

Set the seed supplied to Linux, boot an unchanged shell workload, and read one
`u32` from `/dev/urandom`. Theseus controls the seed, not individual CSPRNG
bytes, so ordinary programs keep using `/dev/urandom` and `/dev/random`.

This tutorial currently requires Linux on arm64, KVM, and Docker. Run every
host command from this directory. Choose a published release and a seed:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
export SEED=42
```

## 1. Inspect the workload and harness

```sh
sed -n '1,160p' init
sed -n '1,240p' run.sh
```

`init` contains the guest shell command. `run.sh` builds its initramfs and
configures Firecracker through the entropy API; it is the reusable low-level
boot harness, not hidden tutorial logic.

The guest operation itself is one ordinary shell read:

```sh
od -An -tu4 -N4 /dev/urandom
```

## 2. Boot with the selected seed

```sh
docker run --rm --privileged --platform linux/arm64 \
  -e SEED -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh ./run.sh
```

Record the printed `chosen random input` value and inspect its retained log:

```sh
grep -a '^chosen random input:' work/guest.log
rm -rf work
docker run --rm --privileged --platform linux/arm64 \
  -e SEED -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh ./run.sh
```

The second value must match the first. Remove `work/` after inspection before
selecting the next seed:

```sh
rm -rf work
```

## 3. Select a different stream

```sh
export SEED=1337
docker run --rm --privileged --platform linux/arm64 \
  -e SEED -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh ./run.sh
```

The new value must differ from the seed-42 value.

## 4. Inspect the retained output

The harness preserves
`work/guest.log`; inspect it before cleanup:

```sh
grep -a '^chosen random input:' work/guest.log
```

## 5. Clean up (optional)

Remove the previous run only when you are ready to repeat:

```sh
rm -rf work
```
