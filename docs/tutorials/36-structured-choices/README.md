# Tutorial 36: Explore choices made by a program

Give a plain C program two bounded choices. Theseus will try their values,
retain each choice at the point where the program uses it, and replay the
combination that exposes a failure.

## Before you start

Use a Linux host with KVM and Docker. Run every command from this directory.
Choose a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
case "$(uname -m)" in
  x86_64) export THESEUS_ARCH=amd64 ;;
  aarch64|arm64) export THESEUS_ARCH=arm64 ;;
  *) echo 'This tutorial requires amd64 or arm64 Linux' >&2; exit 1 ;;
esac
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-${THESEUS_ARCH}
```

## 1. Build the service

```sh
sed -n '1,200p' service/main.c
mkdir -p service/work
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -Wall -Wextra -o service/work/chooser service/main.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -Wall -Wextra -o service/work/ready service/ready.c
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-structured-choice-tutorial service
docker save theseus-structured-choice-tutorial -o service/work/service.tar
```

The program reads the exact assignments from `THESEUS_CHOICES`. It emits one
`THES:CHOICE` record immediately before using each value. This protocol is
language-neutral; it does not require the Theseus SDK.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–5 inside this shell.

## 3. Prepare and inspect the locked choices

```sh
mkdir -p service/work/runtime service/work/guest
cp /usr/local/bin/firecracker service/work/runtime/firecracker
cp /usr/local/bin/theseus-image service/work/runtime/theseus-image
cp /opt/theseus/vmlinux service/work/guest/vmlinux
theseus compose plan > plan.json
grep -n 'guidance\|choice_bounds\|choices\|THESEUS_CHOICES' plan.json
```

The bounds `mode: 2` and `retry: 3` produce six locked assignments. The
default `unified` guidance can combine choice, schedule, coverage, property,
fault, and topology-state feedback when a campaign contains those decisions.

## 4. Run the exploration

```sh
theseus compose explore --expect-counterexample corrupt_result_is_unreachable \
  --output campaign compose.yaml
grep -n 'structured_choices\|corrupt_result_is_unreachable' \
  campaign/campaign-result.json
```

The failure needs `mode=1` and `retry=2`. The result records both values,
bounds, service, operation boundary, and observation order.

## 5. Inspect, minimize, and replay the failure

```sh
theseus report --format markdown --output report/report.md campaign
grep -n 'Structured choices\|corrupt_result_is_unreachable' report/report.md
theseus compose explore --minimize campaign \
  --expect-counterexample corrupt_result_is_unreachable --output minimized
theseus compose replay minimized --output rerun
grep -R 'THES:CHOICE:mode:2:1\|THES:CHOICE:retry:3:2\|"status":"corrupt"' \
  rerun/services/chooser/serial.log
```

Replay rejects a changed assignment, bound, or emitted choice record.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work plan.json campaign report minimized rerun
```
