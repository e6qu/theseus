# Tutorial 43: Serve a campaign registry over HTTP

Explore one workload under two guidance modes, name both campaigns in a
versioned registry, and walk the retained evidence over one local
read-only HTTP surface: index, result, report, live progress, moment and
event queries, history, comparison, and the bundle tree. The directory
ships a recorded example under `recorded/`, so the serving and inspection
steps run with only the published binary - a KVM host regenerates real
evidence in the same places.

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
  -t theseus-serve-registry-tutorial service
docker save theseus-serve-registry-tutorial -o service/work/service.tar
```

The program is the same bounded-choice workload tutorial 36 uses: it reads
`THESEUS_CHOICES` and emits one `THES:CHOICE` record immediately before using
each value.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run step 3 inside this shell.

## 3. Prepare two campaigns and the registry

```sh
mkdir -p service/work/runtime service/work/guest
cp /usr/local/bin/firecracker service/work/runtime/firecracker
cp /usr/local/bin/theseus-image service/work/runtime/theseus-image
cp /opt/theseus/vmlinux service/work/guest/vmlinux
theseus compose explore --guidance unified --max-runs 6 \
  --expect-counterexample corrupt_result_is_unreachable \
  --output recorded/campaign-unified compose.yaml
theseus compose explore --guidance coverage --max-runs 6 \
  --expect-counterexample corrupt_result_is_unreachable \
  --output recorded/campaign-coverage compose.yaml
grep -n 'guidance' recorded/campaign-unified/campaign-result.json \
  recorded/campaign-coverage/campaign-result.json
```

On a host without KVM, skip the explorations: the directory ships a
recorded example under `recorded/` and `registry.json` already names it.
Both explorations run the same candidate corpus at one fixed budget, so the
rows differ only in policy and outcome - exactly what `evaluate compare`
requires. Registry directories resolve from the registry file's directory.

## 4. Run the read-only HTTP surface

Serving needs only the published binary - no KVM, no Docker. From the
tutorial directory on your host:

```sh
theseus serve --index registry.json --address 127.0.0.1:8098 &
sleep 1
curl -s http://127.0.0.1:8098/ ; echo
```

The serve surface is strictly read-only: every non-GET method is refused,
nothing is ever written, and it stops when you interrupt it. The index page
labels each bundle with its kind - both entries here are campaigns.

## 5. Inspect the evidence over HTTP

```sh
curl -s http://127.0.0.1:8098/unified/progress | head -6
curl -s http://127.0.0.1:8098/unified/query/moments
curl -s http://127.0.0.1:8098/history/properties | grep -n 'consistent\|corrupt'
curl -s http://127.0.0.1:8098/unified/tree
```

The journal streams the live reuse curve (`theseus-progress-v1`,
`theseus-run-record-v1`, and `theseus-checkpoint-ledger-v1` lines); the
moment index addresses every operation boundary; the history routes
aggregate the whole served set. Fetch one rendered report and the
side-by-side comparison:

```sh
curl -s http://127.0.0.1:8098/unified/report | grep -n 'Structured choices'
curl -s 'http://127.0.0.1:8098/compare?campaigns=unified,coverage' \
  | grep -n '"guidance"\|"corpus"\|"budget"'
kill %1
```

The comparison applies the CLI's rules: both campaigns explored the same
corpus at the same budget, so the rows differ only in guidance and outcome.

## 6. Clean up (optional)

```sh
kill %1
rm -rf service/work recorded registry.json
```
