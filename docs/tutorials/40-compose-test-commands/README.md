# Tutorial 40: Explore test templates with automatic faults

Discover two Antithesis-compatible test templates, generate faults from a
two-service topology, find a lost update, minimize it, and replay it.

## Before you start

Use a Linux KVM host with Docker. Run every command from this directory.

```sh
export THESEUS_TAG=<12-character-release-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

## 1. Build the service images

Read the test commands:

```sh
find test-template smoke-template -maxdepth 1 -type f -print -exec sed -n '1,80p' {} \;
```

The filename prefix defines when Theseus may run a command. The image contains
independent `lost-update` and `smoke` templates under
`/opt/antithesis/test/v1/`.

```sh
mkdir -p api/work worker/work
docker build --load --target test-driver -t theseus-template-driver .
docker save theseus-template-driver -o api/work/service.tar
docker build --load --target service -t theseus-template-worker .
docker save theseus-template-worker -o worker/work/service.tar
```

Only the API image contains test commands. The worker is an ordinary service
on the same Compose network.

## 2. Enter Theseus

```sh
docker run --rm -it --privileged \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining commands inside the container.

## 3. Prepare the locked plan

```sh
for service in api worker; do
  mkdir -p "$service/work/runtime" "$service/work/guest"
  cp /usr/local/bin/firecracker "$service/work/runtime/firecracker"
  cp /usr/local/bin/theseus-image "$service/work/runtime/theseus-image"
  cp /opt/theseus/vmlinux "$service/work/guest/vmlinux"
done

theseus compose plan > plan.json
grep -n 'test_templates\|fault_profile\|service_kill\|link_fault' plan.json | head -n 20
```

`fault_profile: standard` expands the locked services, networks, and ordinary
command boundaries into a bounded candidate catalog. It includes service
stop, kill, and restart actions, a bounded CPU throttle and clock-rate window
per image service, asymmetric partitions, latency, loss, duplication,
corruption, bandwidth, MTU, and queue limits, and directed link clogs. It does not add faults to setup,
completion, eventually, or finally commands.

## 4. Run the exploration

```sh
theseus compose explore --expect-counterexample lost_update_is_unreachable \
  --output campaign compose.yaml
grep -R '"value":1,"commits":2' campaign/runs/*/services/api/serial.log
grep -n '"faults"\|"actions"\|"test_template"' campaign/campaign-result.json | head -n 30
```

The explorer selects one template and a bounded set of generated faults for
each timeline. Before an eventually or finally command, Theseus recovers every
active generated fault. The retained result records the selected candidates,
their exact targets, their effects, and their recovery actions.

## 5. Inspect, minimize, and replay

```sh
theseus compose explore --minimize campaign \
  --expect-counterexample lost_update_is_unreachable --output minimized
theseus compose replay minimized --output rerun
grep '"value":1,"commits":2' rerun/services/api/serial.log
```

The minimized plan keeps only the operations and faults needed to reproduce
the failure. Replay applies the same service and link actions at the same
operation barriers.

Render the report:

```sh
theseus report --format markdown --output report/report.md campaign
sed -n '1,180p' report/report.md
```

## 6. Clean up (optional)

```sh
exit
rm -rf api/work worker/work plan.json campaign minimized rerun report
```
