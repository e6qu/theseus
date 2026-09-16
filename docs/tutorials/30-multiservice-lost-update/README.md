# Tutorial 30: Reproduce a lost update across services

Run two ordinary worker images against one counter image. The workers use the
simulated Compose network. Before exploring either counter outcome, the
campaign applies a required partition, sends a UDP probe that the simulated
network records as dropped, heals the network, and verifies that HTTP works
again. The workers then remain in flight at the same time. A sequential
schedule leaves the counter at two. An overlapping schedule lets both HTTP
requests read zero before either writes, leaving the counter at one.

This example also exercises one locked runtime contract: service-name
networking, health checks, launch overrides, numeric credentials, environment
precedence, configs, secrets, seeded bind mounts, read-only roots, tmpfs, and
CPU and memory quantities. The images contain no Theseus SDK.

## Before you start

Use a Linux host with KVM and Docker. Run every host command from this
tutorial directory. Choose a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
case "$(uname -m)" in
  x86_64) export THESEUS_ARCH=amd64 ;;
  aarch64|arm64) export THESEUS_ARCH=arm64 ;;
  *) echo 'This tutorial requires amd64 or arm64 Linux' >&2; exit 1 ;;
esac
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-${THESEUS_ARCH}
```

This directory is the complete tutorial input. The two `increment` files are
the worker client and the deliberately unsafe counter endpoint. There is no
orchestration script and no repository checkout.

## 1. Build and inspect the services

Read the race and the worker's runtime checks:

```sh
sed -n '1,100p' counter/increment
sed -n '1,100p' worker/increment
sed -n '1,100p' worker/check
```

Build and save both images for the native runtime architecture:

```sh
name=theseus-multiservice-lost-update
mkdir -p counter/work worker/work
docker build --load --platform "linux/$THESEUS_ARCH" -t "${name}-counter" counter
docker build --load --platform "linux/$THESEUS_ARCH" -t "${name}-worker" worker
docker save "${name}-counter" -o counter/work/counter.tar
docker save "${name}-worker" -o worker/work/worker.tar
```

Inspect the combined Compose contract before running it:

```sh
sed -n '1,220p' compose.yaml
```

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–6 inside this shell.

## 3. Prepare and inspect the locked plan

Both worker services use the same immutable image and manifest. Each receives
its own VM, address, process namespace, and service-local `increment` process.

```sh
for service in counter worker; do
  mkdir -p "$service/work/runtime" "$service/work/guest"
  cp /usr/local/bin/firecracker "$service/work/runtime/firecracker"
  cp /usr/local/bin/theseus-image "$service/work/runtime/theseus-image"
  cp /opt/theseus/vmlinux "$service/work/guest/vmlinux"
done
theseus compose plan > plan.json
grep -n 'writer-a\|writer-b\|10\.1\.0\|shell_phase\|read_only' plan.json
```

Planning has now hashed the runtime, kernel, source images, and every injected
file. The retained replay uses its locked copies instead of these source files.

## 4. Run the expected counterexample

Theseus boots the three services once and captures their ready state, RAM,
and network queues. Every timeline inherits this state. Replay checks resumed
execution; it does not prove repeatable boot.

```sh
theseus compose explore \
  --expect-counterexample distributed_lost_update_is_unreachable \
  --output campaign compose.yaml
grep -n 'distributed_lost_update_is_unreachable' campaign/campaign-result.json
grep -n 'backplane:partition@setup\|backplane:heal@probe_partition' \
  campaign/campaign-result.json
grep -E '"dropped": [1-9][0-9]*' campaign/campaign-result.json
grep -R '"network":"recovered"' campaign/runs/*/services/writer-a/serial.log
grep -R '"value":1' campaign/runs/*/services/counter/serial.log
grep -R '"value":2' campaign/runs/*/services/counter/serial.log
```

The command succeeds only when the named property has a retained failed
verdict. The action names, dropped-frame count, and recovery output show that
the partition was exercised and healed before the two counter outcomes were
explored.

## 5. Inspect, minimize, and replay

Inspect which service received each operation and where network frames crossed
an operation boundary:

```sh
grep -n 'op-[0-9][0-9][0-9]-\|network_traffic_delta\|writer-a\|writer-b' \
  campaign/campaign-result.json
```

Minimize only the named failure, then replay its locked topology:

```sh
theseus compose explore --minimize campaign \
  --expect-counterexample distributed_lost_update_is_unreachable \
  --output minimized
sed -n '1,180p' minimized/minimization.json
theseus compose replay minimized --output rerun
theseus compose verify campaign
theseus compose verify minimized
theseus compose verify rerun
grep -n 'partition\|heal' rerun/topology-result.json
grep -E '"dropped": [1-9][0-9]*' rerun/services/*/result.json
grep -R '"network":"recovered"' rerun/services/writer-a/serial.log
grep -R '"value":1' rerun/services/counter/serial.log
```

Required actions survive minimization, so this replay includes the partition,
dropped UDP probe, recovery, successful HTTP probe, and lost update. The replay
uses the minimized plan and artifacts. It does not rebuild either image or
select a new operation schedule. Its artifact paths are relative to the locked
bundle, so the complete `minimized` directory can be moved and replayed
elsewhere.

`compose verify` checks locked inputs, RAM, ancestry, logs, and full execution
hashes without KVM. It does not certify native execution. Treat retained RAM
as sensitive: it can contain application secrets.

## 6. Clean up (optional)

```sh
find campaign minimized rerun \( -name replay-plan.json -o -name campaign-result.json \)
exit
```

Keep the directories while investigating. Remove generated data on the host
only when you no longer need the evidence:

```sh
rm -rf counter/work worker/work plan.json campaign minimized rerun
```
