# Tutorial 41: Inspect low-level execution

Run an ordinary BusyBox HTTP service, inspect its per-vCPU exit streams and
machine-wide execution stream, then compare those observations with a replay.
The service uses no Theseus SDK or instrumentation.

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

Theseus retains a checkpoint at service readiness. The campaign and replay
inherit its boot prefix and check resumed execution. They do not prove
repeatable fresh boot.

```sh
theseus compose explore --output campaign compose.yaml
```

## 5. Inspect and replay the campaign

```sh
grep -E '"execution_decisions": [1-9][0-9]*' campaign/campaign-result.json
grep -n -A8 '"execution_ledgers"' campaign/campaign-result.json
grep -n -A8 '"machine_execution_ledgers"' campaign/campaign-result.json
grep -n -A4 '"machine_execution_traces"' campaign/campaign-result.json
grep -m1 '"host:serial_input:' campaign/campaign-result.json
grep -m1 '"vcpu:0:interrupt:serial:' campaign/campaign-result.json
grep -m1 '"vcpu:0:interrupt:virtio-' campaign/campaign-result.json
theseus compose replay campaign --output rerun
theseus compose verify campaign
theseus compose verify rerun
grep -A2 '"replay_verification"' rerun/campaign-result.json
theseus compare campaign rerun > comparison.json
grep -E '"status": "(same|diverged)"' comparison.json
```

Each ledger contains a decision count, a SHA-256 identity for the complete
stream, and the last 32 readable decisions. Per-vCPU ledgers preserve local
order. The machine trace prefixes guest exits with `vcpu:` and the HTTP
operation's UART command with `host:`. It preserves the complete observed
order. The `interrupt:serial:` and `interrupt:virtio-` records show
that UART and service-device requests were injected on recorded vCPU turns
instead of racing through asynchronous irqfds. The machine ledger is the
compact digest and readable tail of this stream.

Checkpoint-backed campaigns use the portable `host_inputs` replay contract.
Replay restores the retained ready state, reapplies the same UART command, and
requires the declared operation and property to pass. It retains the new KVM
stream as evidence, but it does not require Linux to take the same exits or
service interrupts on the same turns. `theseus compare` reports `same` when
the observations match and `diverged` with the first difference otherwise;
either result is valid for this campaign contract.

Exact fixed-run replay is stricter: it installs the complete retained machine
trace, admits only the recorded vCPU at each emulated device effect, and rejects
the first changed turn, input, or value. That still does not control guest
instruction scheduling between KVM exits.

Keep the entire `campaign` directory, including RAM and locked artifacts.
`compose verify` checks its integrity offline; it does not certify native
execution. Checkpoints may contain application secrets retained in guest RAM.

## 6. Clean up (optional)

```sh
exit
rm -rf api/work plan.json campaign rerun comparison.json
```
