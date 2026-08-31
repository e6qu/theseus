# Tutorial 1: Run and replay the Theseus service

## Objective

Run Theseus through its public host surface: the `firecracker` service and
its Unix-socket HTTP API. You will boot the same plain C program three times,
configure its entropy with `PUT /entropy`, and prove that one seed gives one
replayable byte stream. No Theseus SDK is involved yet.

## What you operate

Theseus currently exposes its host interface through the forked
`firecracker` binary:

| You do this | The service receives |
|---|---|
| Start `firecracker --api-sock /tmp/theseus.sock` | a Unix-socket HTTP server |
| `PUT /boot-source` | kernel and initramfs paths |
| `PUT /machine-config` | vCPU and memory settings |
| `PUT /entropy` | the seed for guest-visible entropy |
| `PUT /actions` with `InstanceStart` | the command to boot |

The guest is a small ordinary C program in [init.c](init.c). It reads
`/dev/hwrng`, prints the bytes, and powers off. The companion
[run.sh](run.sh) performs the API sequence above three times so that the
experiment is repeatable.

Once the kernel and C initramfs exist, one service boot is the following
sequence (the script supplies the concrete paths and waits for the socket):

```sh
"$FC" --api-sock "$SOCK" --no-seccomp &
curl --unix-socket "$SOCK" -X PUT localhost/boot-source \
  -H 'Content-Type: application/json' \
  -d "{\"kernel_image_path\": \"$KERNEL\", \"initrd_path\": \"$INITRAMFS\", \"boot_args\": \"console=ttyS0 reboot=k panic=-1\"}"
curl --unix-socket "$SOCK" -X PUT localhost/machine-config \
  -H 'Content-Type: application/json' -d '{"vcpu_count": 1, "mem_size_mib": 128}'
curl --unix-socket "$SOCK" -X PUT localhost/entropy \
  -H 'Content-Type: application/json' -d '{"seed": 42}'
curl --unix-socket "$SOCK" -X PUT localhost/actions \
  -H 'Content-Type: application/json' -d '{"action_type": "InstanceStart"}'
```

## Setup

You need Linux with KVM. On Apple Silicon macOS, start a privileged aarch64
container from the repository root:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0-bookworm bash
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev gcc cpio curl
cd /theseus/firecracker && cargo build -p firecracker
```

## Run the service experiment

From the repository root inside the container:

```sh
sh /theseus/docs/tutorials/01-replay-by-seed/run.sh
```

The script downloads a small Linux kernel on its first run, compiles
`init.c` into an initramfs, then starts the service with seeds `42`, `42`,
and `1337`. Its output includes:

```
seed 42:   hwrng (64 bytes): 28065689f706c281a35be8609b92dce6...
seed 42:   hwrng (64 bytes): 28065689f706c281a35be8609b92dce6...
seed 1337: hwrng (64 bytes): 70788b9d6210d1870cda0f02887c9e28...
PASS: identical across same-seed boots, different across seeds
```

The first two runs are the same because `PUT /entropy` gave the service the
same seed. The third differs because only that seed changed.

## What you have now

You can drive Theseus without linking a guest library: launch the service,
configure a boot through its API, retain a seed, and replay the observable
guest entropy exactly. Next, send a chosen input stream rather than merely
seeding one.

## Further reading

- [determinism.md](../../determinism.md) — what is seeded and what remains
  outside the replay boundary
- [The Firecracker API](../../../firecracker/docs/api_requests/) — the
  complete host API inherited by the fork
