# Tutorial 2: Script a guest through the service API

## Objective

Choose the values a guest sees without modifying that guest or linking an
SDK. You will configure the Theseus service's `PUT /entropy` endpoint with a
byte script, boot a plain C program, and see it print `1 2 3 4`.

## The service control

`PUT /entropy` accepts both a fallback seed and a `script`. The service
returns script bytes verbatim before continuing with the seeded ChaCha stream:

```json
{ "seed": 42, "script": [1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0] }
```

Those bytes encode four little-endian `u64` values. The guest in
[init.c](init.c) is plain C: it opens `/dev/hwrng`, reads four `u64`s, prints
them, and exits. It has no Theseus dependency.

The call that changes the guest's behavior is an ordinary API request:

```sh
curl --unix-socket "$SOCK" -X PUT localhost/entropy \
  -H 'Content-Type: application/json' \
  -d "{\"seed\": 42, \"script\": [$SCRIPT]}"
```

The tutorial script constructs `$SCRIPT` from the JSON bytes above after a
small boot-time prefix consumed by Linux's entropy initialization.

## Setup

You need Linux with KVM. On Apple Silicon macOS, run this from the repository
root:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0-bookworm bash
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev gcc cpio curl
cd /theseus/firecracker && cargo build -p firecracker
```

## Run it

```sh
sh /theseus/docs/tutorials/02-control-the-random/run.sh
```

The script builds the C initramfs, starts `firecracker --api-sock`, configures
the boot and machine, sends the entropy JSON above, and starts the VM. You
will see:

```
random() = 1 2 3 4
PASS: random() returned 1, 2, 3, 4 — the values we scripted
```

The script deliberately reserves the bytes the Linux kernel consumes while
seeding itself, then places the four values where the C program's read will
land. Once the script is consumed, the seed supplies reproducible fallback
bytes.

## What you have now

You can force a retry count, timeout choice, or randomized branch from the
host API alone. The next tutorial is for the point where you need the guest to
report a meaningful outcome, rather than only print a value.

## Further reading

- [determinism.md](../../determinism.md) — scripted and seeded entropy
- [The Firecracker API](../../../firecracker/docs/api_requests/) — the
  complete host API inherited by the fork
