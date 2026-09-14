# Tutorial 31: Guide a campaign with C basic-block coverage

Compile an ordinary C command with the instrumentation shipped in the Theseus
runtime. Run three inputs and inspect the stable application-block identities
that guide the campaign. The command does not link the Theseus SDK.

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

## 1. Build and inspect the instrumented command

Read the branch you will exercise:

```sh
sed -n '1,120p' service/main.c
```

Use the compiler frontend from the published runtime. It records the compiler,
target, source and preprocessed-input hashes, process name, module name, and
build identity next to the binary.

```sh
mkdir -p service/work
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/c/theseus-coverage-cc \
  --process classifier --module branching -o service/work/classify service/main.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -o service/work/ready service/ready.c
sed -n '1,120p' service/work/classify.theseus-coverage.json
```

```sh
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-c-coverage-tutorial service
docker save theseus-c-coverage-tutorial -o service/work/service.tar
sed -n '1,160p' compose.yaml
```

The Compose extension selects `application_blocks`. Each record is scoped by
service, process, module, build SHA-256, and module-relative block address, so
ASLR cannot change its identity and a different build cannot be conflated with
this one.

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
grep -n 'application_blocks' plan.json
```

## 4. Run the campaign

```sh
theseus compose explore --output campaign compose.yaml
```

## 5. Inspect and replay the coverage

```sh
grep -n 'unique_application_blocks\|application_block_novelty' \
  campaign/campaign-result.json
grep -R '^THES:COV:v1:classifier:branching:' campaign/runs/*/services/classifier/serial.log
theseus report campaign --output report
grep -n 'application block' report/report.md
theseus compose replay campaign --output rerun
grep -n '"status": "passed"' rerun/campaign-result.json
```

The replay succeeds only if the recorded application-block sets and novelty
match. The raw serial records, structured campaign result, locked service
image, and human-readable report remain available for inspection.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work plan.json campaign report rerun
```
