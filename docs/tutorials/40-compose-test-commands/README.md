# Tutorial 40: Run test templates

Package two independent test templates in one image. Let Theseus discover both
and select one for each generated timeline. The directory layout also works
with Antithesis.

## Before you start

Use a Linux KVM host with Docker. Run every command from this directory.

```sh
export THESEUS_TAG=<12-character-release-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

## 1. Build the image

Read both templates:

```sh
find test-template smoke-template -maxdepth 1 -type f -print -exec sed -n '1,80p' {} \;
```

The filename prefix is the lifecycle contract. `first_` prepares state,
`parallel_driver_` may run concurrently, `serial_driver_` runs without a live
parallel driver, and `eventually_` or `finally_` ends a timeline.

```sh
name=theseus-test-template-tutorial
mkdir -p api/work
docker build --load -t "$name" .
docker save "$name" -o api/work/service.tar
```

The Dockerfile installs `lost-update` and `smoke` under
`/opt/antithesis/test/v1/`. Theseus reads both directories from the saved
image. There is no command list or orchestration script to maintain.

## 2. Enter Theseus

```sh
docker run --rm -it --privileged \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining commands inside the container.

## 3. Prepare the locked plan

```sh
mkdir -p api/work/runtime api/work/guest
cp /usr/local/bin/firecracker api/work/runtime/firecracker
cp /usr/local/bin/theseus-image api/work/runtime/theseus-image
cp /opt/theseus/vmlinux api/work/guest/vmlinux
theseus compose plan > plan.json
grep -n 'test_templates\|test_command_path\|shell_process' plan.json
```

The plan lists `lost-update` and `smoke`. Operations are scoped to one of them;
Theseus never mixes their commands in one timeline.

`max_parallel_commands: 2` gives each parallel command two process slots. The
explorer decides how many to start and when to join them.

## 4. Run the campaign

```sh
theseus compose explore --expect-counterexample lost_update_is_unreachable \
  --output campaign compose.yaml
grep -R '"value":1,"commits":2' campaign/runs/*/services/api/serial.log
grep -n '"test_template"' campaign/campaign-result.json | head
```

Two writers read zero before either writes. Both then write one. The serial
command records two completed writes and the incorrect final value. The result
also records which template produced every timeline.

## 5. Inspect, minimize, and replay the failure

```sh
theseus compose explore --minimize campaign \
  --expect-counterexample lost_update_is_unreachable --output minimized
theseus compose replay minimized --output rerun
grep '"value":1,"commits":2' rerun/services/api/serial.log
```

An `eventually_` command may instead start while drivers are live. Theseus
kills those commands across their image services and restores active campaign
faults before running the eventual check. A `finally_` command waits for every
started command to finish.

Render the report:

```sh
theseus report campaign --output report
sed -n '1,180p' report/report.md
```

## 6. Clean up (optional)

Keep the four evidence directories while investigating. Remove generated files
when you no longer need them:

```sh
exit
rm -rf api/work plan.json campaign minimized rerun report
```
