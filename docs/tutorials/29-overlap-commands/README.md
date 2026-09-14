# Tutorial 29: Find a lost update with overlapping commands

Run two ordinary processes inside one unmodified container image. A sequential
schedule increments a counter twice. An overlapping schedule lets both writers
read zero before either writes, so the final value is one. Theseus records the
launches, completion-observation order, assertion, and minimized failing
schedule.

## Before you start

Use a Linux host with KVM and Docker. Run every host command from this
tutorial directory. Choose a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

This directory is the complete tutorial input. `writer` is the actual buggy
workload; there is no orchestration wrapper and no repository checkout.

## 1. Build and inspect the workload

Read the process that performs the unsafe read-modify-write:

```sh
sed -n '1,80p' writer
```

Build and save the service image:

```sh
name=theseus-overlap-tutorial
mkdir -p api/work
docker build --platform linux/arm64 -t "$name" .
docker save "$name" -o api/work/service.tar
```

Read the command lifecycle before running it:

```sh
sed -n '15,170p' compose.yaml
```

`phase: launch` starts a named process without waiting. `phase: completion`
joins it later. Setup, assertion, and recovery commands run synchronously.
Theseus rejects a completion before its matching launch and a second launch of
the same live process.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–6 inside this shell.

## 3. Prepare locked runtime inputs

```sh
mkdir -p api/work/runtime api/work/guest
cp /usr/local/bin/firecracker api/work/runtime/firecracker
cp /usr/local/bin/theseus-image api/work/runtime/theseus-image
cp /opt/theseus/vmlinux api/work/guest/vmlinux
theseus compose plan > plan.json
```

Inspect the normalized process lifecycle:

```sh
grep -n 'shell_phase\|shell_process' plan.json
```

## 4. Run the campaign

Require the campaign to retain the deliberately false
`lost_update_is_unreachable` property:

```sh
theseus compose explore --expect-counterexample lost_update_is_unreachable \
  --output campaign compose.yaml
grep -n '"status": "failed"' campaign/campaign-result.json
grep -n 'lost_update_is_unreachable' campaign/campaign-result.json
grep -R '"value":1' campaign/runs/*/services/api/serial.log
grep -R '"value":2' campaign/runs/*/services/api/serial.log
```

The last two commands show both the overlapping failure and the sequential
passing result in the retained run corpus.

## 5. Inspect, minimize, and replay the failure

```sh
theseus compose explore --minimize campaign \
  --expect-counterexample lost_update_is_unreachable --output minimized
sed -n '1,160p' minimized/minimization.json
theseus compose replay minimized --output rerun
grep -R '"value":1' rerun/services/api/serial.log
```

The replay reproduces the minimized counterexample. Inspect stable boundary
IDs and completion events:

```sh
grep -n 'op-[0-9][0-9][0-9]-\|"completion"\|"process"' \
  campaign/campaign-result.json
grep -n '"phase":"completion"' minimized/services/api/serial.log
```

## 6. Clean up (optional)

```sh
find campaign minimized rerun \( -name replay-plan.json -o -name campaign-result.json \)
exit
```

Keep these directories while investigating. After inspection, optionally
remove generated data on the host:

```sh
rm -rf api/work plan.json campaign minimized rerun
```
