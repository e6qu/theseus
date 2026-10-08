# Tutorial 44: Campaign a rendered Helm chart

Render a one-pod Helm chart with `helm template`, hand Theseus the
rendered directory as the campaign input, and inspect the locked plan -
the same input contract the Kubernetes tutorials explore, without
invoking Helm inside Theseus. The pod carries a workload container and a
log-forwarder sidecar, so the plan also shows the per-container service
contract.

## Before you start

Use a Linux host with Docker, plus the Helm CLI. Run every command from
this directory. Choose a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
case "$(uname -m)" in
  x86_64) export THESEUS_ARCH=amd64 ;;
  aarch64|arm64) export THESEUS_ARCH=arm64 ;;
  *) echo 'This tutorial requires amd64 or arm64 Linux' >&2; exit 1 ;;
esac
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-${THESEUS_ARCH}
```

## 1. Build the service and render the chart

```sh
sed -n '1,60p' service/serve.py
mkdir -p service/work
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  sh -c "apt-get update -qq && apt-get install -y -qq python3 >/dev/null"
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-helm-tutorial service
docker save theseus-helm-tutorial -o service/work/service.tar
helm template chart --output-dir rendered
find rendered -name '*.yaml' | sort
```

The workload is an unmodified HTTP service: one Python endpoint that
answers `/health` with `ok` and echoes `mode` and `retry` query values
back as JSON. The Deployment carries two containers - the workload and a
log-forwarder sidecar, each annotated with its own
`theseus.io/manifest` - and one ClusterIP Service, the documented
Kubernetes subset. Theseus never invokes Helm; the rendered directory is
the input.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3-5 inside this shell.

## 3. Prepare the campaign input

```sh
for service in chooser forwarder; do
  mkdir -p rendered/$service/runtime rendered/$service/guest
  cp /usr/local/bin/firecracker rendered/$service/runtime/firecracker
  cp /usr/local/bin/theseus-image rendered/$service/runtime/theseus-image
  cp /opt/theseus/vmlinux rendered/$service/guest/vmlinux
  cat > rendered/$service/theseus.toml <<MANIFEST
version = 1

[runtime]
firecracker = "runtime/firecracker"
image_adapter = "runtime/theseus-image"

[guest]
kernel = "guest/vmlinux"
image = "/tutorial/service/work/service.tar"

[run]
seed = 42

[container_service.ready]
url = "http://127.0.0.1:8080/health"

[[container_service.operations]]
name = "calculate"
url = "http://127.0.0.1:8080/calculate?mode=1&retry=2"
MANIFEST
done
cat > rendered/campaign.toml <<'CAMPAIGN'
driver = "chooser-chooser"
max_runs = 6
max_operations_per_run = 1

[[operations]]
name = "calculate"
service = "chooser-chooser"

[operations.http]
url = "http://127.0.0.1:8080/calculate?mode=1&retry=2"
CAMPAIGN
theseus compose validate rendered
```

A directory input walks every sorted `.yaml`/`.yml` file - the rendered
Deployment and Service - through the documented Kubernetes subset, and
reads `campaign.toml` (the same shape a Compose file puts under
`x-theseus.campaign`) for the declared campaign. Multi-container pods
translate into one service per container, named `<pod>-<container>`:
`chooser-chooser` for the workload and `chooser-log-forwarder` for the
sidecar, each with its own per-container manifest.

## 4. Run the plan lock

```sh
theseus compose plan rendered > plan.json
grep -c '"chooser-chooser"' plan.json
grep -c '"chooser-log-forwarder"' plan.json
grep -n 'calculate' plan.json | head -4
```

The two greps confirm the per-container contract: both containers
translate into their own service with its own locked manifest.

## 5. Inspect the locked plan and explore

```sh
theseus compose explore --output campaign --max-runs 2 rendered
grep -c 'theseus-checkpoint-ledger-v1' campaign/progress.jsonl
theseus status campaign | grep journal
```

The exploration journals every run as it completes: the progress journal
records the live account (progress lines, run records, checkpoint
economics) while the campaign runs, and `theseus status` reads the same
summary afterwards.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work rendered campaign plan.json
```
