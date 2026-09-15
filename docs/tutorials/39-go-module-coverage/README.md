# Tutorial 39: Cover a Go module

Instrument a Go command and the packages it imports from the same module. Run
three inputs, inspect source-associated block coverage, and replay the campaign.
The program does not use the Theseus SDK.

## Before you start

Use a Linux host with KVM and Docker. Run every command from this tutorial directory.
Select a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
case "$(uname -m)" in
  x86_64) export THESEUS_ARCH=amd64 ;;
  aarch64|arm64) export THESEUS_ARCH=arm64 ;;
  *) echo 'This tutorial requires amd64 or arm64 Linux' >&2; exit 1 ;;
esac
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-${THESEUS_ARCH}
```

## 1. Build the Go module

Read the command and its imported package:

```sh
sed -n '1,160p' service/cmd/classifier/main.go
sed -n '1,120p' service/logic/logic.go
```

Build the command with the published Theseus CLI. This instruments every Go
source package in the main module that the command imports. It works on an
isolated copy and does not edit these source files.

```sh
mkdir -p service/work/symbols
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial/service "$THESEUS_IMAGE" \
  theseus coverage go \
  --package ./cmd/classifier --process classifier --module command \
  --symbols work/symbols --output work/classifier \
  --goarch "$THESEUS_ARCH" --offline
sed -n '1,220p' service/work/classifier.theseus-coverage.json
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/go/theseus-coverage-inspect \
  service/work/classifier service/work/classifier.theseus-coverage.json
```

The manifest must list both `example.com/classifier/cmd/classifier` and
`example.com/classifier/logic`. Preserve the generated symbols, strip the
deployed command, and build its service image:

```sh
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  sh -c 'strip --strip-unneeded service/work/classifier && clang -O2 -o service/work/ready service/ready.c'
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-go-coverage-tutorial service
docker save theseus-go-coverage-tutorial -o service/work/service.tar
sed -n '1,180p' compose.yaml
```

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–5 inside this shell.

## 3. Prepare the locked runtime inputs

```sh
mkdir -p service/work/runtime service/work/guest
cp /usr/local/bin/firecracker service/work/runtime/firecracker
cp /usr/local/bin/theseus-image service/work/runtime/theseus-image
cp /opt/theseus/vmlinux service/work/guest/vmlinux
theseus compose plan > plan.json
grep -n 'application_blocks\|theseus-go-coverage-build-v1\|symbols' plan.json
```

## 4. Run the campaign

```sh
theseus compose explore --output campaign compose.yaml
```

## 5. Inspect and replay the Go coverage

```sh
grep -R '^THES:COV:v1:classifier:command:' \
  campaign/runs/*/services/classifier/serial.log
theseus report --format markdown --output report/report.md campaign
grep -n 'cmd/classifier/main.go\|logic/logic.go' report/report.md
theseus compose replay campaign --output rerun
grep -n '"status": "passed"' rerun/campaign-result.json
```

The report must contain reached lines from the command and its `logic` package.
It uses the symbol file locked beside the campaign, not debug data in the
stripped service binary.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work service/target plan.json campaign report rerun
```
