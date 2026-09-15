# Tutorial 40: Compose a test from lifecycle commands

Assign lifecycle roles to ordinary commands. Theseus runs setup first, overlaps
two drivers, permits an anytime observation, waits for both processes before a
serial command, and ends with a final or eventual check.

## Before you start

Use a Linux host with KVM and Docker. Run every command from this directory.
Choose a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

This directory is the complete input. `writer` is the buggy workload, not an
orchestration wrapper.

## 1. Build the service image

```sh
sed -n '1,80p' writer
name=theseus-test-command-tutorial
mkdir -p api/work
docker build --load -t "$name" .
docker save "$name" -o api/work/service.tar
```

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run the remaining steps inside this shell.

## 3. Prepare the locked plan

Read the template before running it:

```sh
sed -n '14,150p' compose.yaml
```

Each operation has one `command` role:

- `first` runs once before the workload.
- `parallel_driver` may overlap named processes through `phase: launch` and
  `phase: completion`.
- `serial_driver` runs only after parallel processes have completed.
- `anytime` may run while a driver process is live.
- `eventually` and `finally` are terminal checks; campaign actions cannot be
  attached after them.

The same model also accepts `singleton_driver` for one exclusive driver
timeline. A campaign must assign a role to every operation when it uses this
model.

```sh
mkdir -p api/work/runtime api/work/guest
cp /usr/local/bin/firecracker api/work/runtime/firecracker
cp /usr/local/bin/theseus-image api/work/runtime/theseus-image
cp /opt/theseus/vmlinux api/work/guest/vmlinux
theseus compose plan > plan.json
grep -n '"command"' plan.json
```

The plan retains every lifecycle role. Invalid setup order, an unjoined
parallel process, and a serial driver beside a live parallel process are absent
from the generated corpus.

## 4. Run the test template

```sh
theseus compose explore --expect-counterexample lost_update_is_unreachable \
  --output campaign compose.yaml
```

## 5. Inspect, minimize, and replay the failure

```sh
grep -R '"value":1' campaign/runs/*/services/api/serial.log
grep -n 'command:parallel_driver\|command:serial_driver' \
  campaign/campaign-result.json

theseus compose explore --minimize campaign \
  --expect-counterexample lost_update_is_unreachable --output minimized
theseus compose replay minimized --output rerun
grep '"value":1' rerun/services/api/serial.log
```

Both writers read zero before release, so both write one. The replay uses the
locked lifecycle decisions and reproduces that final value.

```sh
theseus report campaign --output report
grep -n 'Test command\|parallel_driver\|finally' report/report.md
```

## 6. Clean up (optional)

Keep the evidence while investigating. List it and leave the runtime shell:

```sh
find campaign minimized rerun report -maxdepth 1 -type f
exit
```

Remove it only when you no longer need it:

```sh
rm -rf api/work plan.json campaign minimized rerun report
```
