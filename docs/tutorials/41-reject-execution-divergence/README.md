# Tutorial 41: Reject low-level execution divergence

Run an ordinary BusyBox HTTP service, inspect its per-vCPU and machine-wide
KVM-exit streams, and replay them exactly. The service uses no Theseus SDK or
instrumentation.

## Before you start

Use an amd64 or arm64 Linux host with KVM and Docker. Run every command from
this directory. Select a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
case "$(uname -m)" in
  x86_64) export THESEUS_ARCH=amd64 ;;
  aarch64|arm64) export THESEUS_ARCH=arm64 ;;
  *) echo 'This tutorial requires amd64 or arm64 Linux' >&2; exit 1 ;;
esac
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-${THESEUS_ARCH}
```

## 1. Build the service image

```sh
sed -n '1,80p' Dockerfile
mkdir -p api/work
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-strict-execution-tutorial .
docker save theseus-strict-execution-tutorial -o api/work/service.tar
```

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–5 inside this shell.

## 3. Prepare the locked runtime

```sh
sed -n '1,160p' compose.yaml
sed -n '1,160p' api/theseus.toml
mkdir -p api/work/runtime api/work/guest
cp /usr/local/bin/firecracker api/work/runtime/firecracker
cp /usr/local/bin/theseus-image api/work/runtime/theseus-image
cp /opt/theseus/vmlinux api/work/guest/vmlinux
theseus compose plan > plan.json
```

## 4. Run the campaign

```sh
theseus compose explore --output campaign compose.yaml
```

## 5. Inspect and replay the exact path

```sh
grep -E '"execution_decisions": [1-9][0-9]*' campaign/campaign-result.json
grep -n -A8 '"execution_ledgers"' campaign/campaign-result.json
grep -n -A8 '"machine_execution_ledgers"' campaign/campaign-result.json
grep -n -A4 '"machine_execution_traces"' campaign/campaign-result.json
theseus compose replay campaign --output rerun
grep -A2 '"replay_verification"' rerun/campaign-result.json
theseus compare campaign rerun > comparison.json
grep '"status": "same"' comparison.json
```

Each ledger contains a decision count, a SHA-256 identity for the complete
stream, and the last 32 readable decisions. Per-vCPU ledgers preserve local
order. The exact machine trace prefixes each decision with its vCPU and
preserves the complete order needed to drive replay. The machine ledger is its
compact digest and readable tail.

Before a restored campaign leaf runs, Theseus validates its inherited
checkpoint prefix and installs the remaining exact trace. At each emulated
device effect it admits only the recorded vCPU and verifies the exit kind,
address, width, and payload. Replay fails on the first wrong turn or value,
even when final HTTP and serial output might otherwise look the same. This does
not control instruction scheduling between KVM exits.

## 6. Clean up (optional)

```sh
exit
rm -rf api/work plan.json campaign rerun comparison.json
```
