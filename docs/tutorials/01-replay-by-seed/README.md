# Tutorial 1: Replay Linux random devices

Boot the same tiny Linux guest three times. The guest reads `/dev/urandom`
and `/dev/random` with ordinary shell commands. Two boots use seed `42`; the
third uses seed `1337`.

This tutorial currently requires Linux on arm64, KVM, and Docker. Run every
host command from this directory. Choose a published 12-character commit SHA:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
```

The local `init` file is the complete guest workload. Read it first:

```sh
sed -n '1,160p' init
```

It loads the published deterministic-CRNG kernel module and uses BusyBox `od`
to print 16 bytes from each standard random device. It does not use an SDK.
The meaningful guest commands are simply:

```sh
od -An -tx1 -N16 /dev/urandom
od -An -tx1 -N16 /dev/random
```

## 1. Inspect the Firecracker harness

`run.sh` is not an application wrapper. It is the low-level host harness that
builds the initramfs and issues Firecracker API calls for three boots. Review
those API calls before running them:

```sh
sed -n '1,260p' run.sh
```

## 2. Run the three boots

```sh
docker run --rm --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh ./run.sh
```

The command prints three pairs of random-device values and ends with:

```text
PASS: both standard random devices replay by seed
```

The two seed-42 lines must match. The seed-1337 line must differ. Keep the
seed with a failure; it is the replay input.

## 3. Inspect the three serial logs

```sh
grep -aE '^(urandom|random):' work/first.log work/second.log work/third.log
```

## 4. Clean up (optional)

```sh
rm -rf work
```
