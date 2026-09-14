# Tutorial 37: Guide a campaign with LLVM edge coverage

Instrument a C++ command and a dynamically loaded C library with the LLVM
tools shipped in the Theseus runtime. Run three inputs, then inspect and replay
the edge coverage that guided the campaign. Neither module uses the Theseus SDK.

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

## 1. Build and inspect both instrumented modules

Read the command and its plugin:

```sh
sed -n '1,160p' service/main.cc
sed -n '1,80p' service/plugin.c
```

Compile them with the frontend in the published runtime. Keep their unstripped
symbol files in the image under `/symbols`.

```sh
mkdir -p service/work/symbols
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/llvm/theseus-coverage-clang \
  --process classifier --module plugin --shared --symbols service/work/symbols \
  -o service/work/plugin.so service/plugin.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/llvm/theseus-coverage-clang \
  --process classifier --module command --language c++ --symbols service/work/symbols \
  -o service/work/classify service/main.cc -- -ldl
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  clang -O2 -o service/work/ready service/ready.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/llvm/theseus-coverage-inspect \
  service/work/classify service/work/classify.theseus-coverage.json
```

The last command must report a nonzero guard count. Each manifest locks the
language, compiler, target, sources, module name, and build identity.

```sh
sed -n '1,160p' service/work/classify.theseus-coverage.json
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-llvm-coverage-tutorial service
docker save theseus-llvm-coverage-tutorial -o service/work/service.tar
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
grep -n 'application_edges' plan.json
```

## 4. Run the campaign

```sh
theseus compose explore --output campaign compose.yaml
```

## 5. Inspect and replay the edges

```sh
grep -R '^THES:COV:v2:classifier:' campaign/runs/*/services/classifier/serial.log
grep -n 'unique_application_edges\|application_block_novelty' \
  campaign/campaign-result.json
edge_offset=$(awk -F: '/^THES:COV:v2:classifier:command:/ {print $8; exit}' \
  campaign/runs/*/services/classifier/serial.log)
command_symbols=$(find service/work/symbols -name 'command-*.debug' -print -quit)
/opt/theseus/instrumentation/llvm/theseus-coverage-inspect \
  "$command_symbols" service/work/classify.theseus-coverage.json "$edge_offset"
theseus report campaign --output report
grep -n 'LLVM-instrumented application edge' report/report.md
theseus compose replay campaign --output rerun
grep -n '"status": "passed"' rerun/campaign-result.json
```

Records from both `command` and `plugin` must appear. The inspection command
resolves one reached command edge to its function and source line. The edge
number is scoped by the module build SHA-256, and the module-relative address
remains stable under ASLR.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work plan.json campaign report rerun
```
