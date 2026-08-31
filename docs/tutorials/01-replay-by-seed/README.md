# Tutorial 1: Run and replay a VM

Boot a plain C guest through the Theseus service. Run it twice with the same
entropy seed and once with a different seed. The first two runs match; the
third does not.

## 1. Start a Linux+KVM environment

From the repository root on Apple Silicon:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0-bookworm bash
```

Inside the container:

```sh
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev gcc cpio curl
cd /theseus/firecracker && cargo build -p firecracker
```

Keep this container open for tutorials 2–4.

## 2. Run the example

```sh
sh /theseus/docs/tutorials/01-replay-by-seed/run.sh
```

The script builds [init.c](init.c), a C program that reads `/dev/hwrng` and
prints the result. It boots that guest with seeds `42`, `42`, and `1337`.

```text
seed 42:   hwrng (64 bytes): 28065689f706c281...
seed 42:   hwrng (64 bytes): 28065689f706c281...
seed 1337: hwrng (64 bytes): 70788b9d6210d187...
PASS: identical across same-seed boots, different across seeds
```

## 3. See the host API

The script starts the service with `firecracker --api-sock` and configures it
through four requests:

```text
PUT /boot-source     kernel and initramfs
PUT /machine-config  vCPUs and memory
PUT /entropy         seed 42
PUT /actions         InstanceStart
```

Keep the seed with a failed run. It is the replay recipe. Next, replace the
seeded stream with values you choose.

See [the full API](../../../firecracker/docs/api_requests/) or
[the determinism boundary](../../determinism.md).
