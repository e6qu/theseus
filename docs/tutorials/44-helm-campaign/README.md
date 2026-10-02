# Tutorial 44: Validate a rendered Helm chart

Render a one-service Helm chart with `helm template`, hand Theseus the
rendered directory as the campaign input, and inspect the locked plan -
the same input contract the Kubernetes tutorials explore, without
invoking Helm inside Theseus.

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
back as JSON. The chart renders one Deployment and one ClusterIP Service
- the documented Kubernetes subset. Theseus never invokes Helm; the
rendered directory is the input.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3-5 inside this shell.

## 3. Prepare the campaign input

```sh
cat > rendered/theseus.toml <<'MANIFEST'
version = 1

[run]
seed = 42

[container_service.ready]
url = "http://127.0.0.1:8080/health"

[[container_service.operations]]
name = "calculate"
url = "http://127.0.0.1:8080/calculate?mode=1&retry=2"
MANIFEST
cat > rendered/campaign.toml <<'CAMPAIGN'
driver = "chooser"
max_runs = 6
max_operations_per_run = 1

[[operations]]
name = "calculate"
service = "chooser"

[operations.http]
url = "http://127.0.0.1:8080/calculate?mode=1&retry=2"
CAMPAIGN
theseus compose validate rendered
```

A directory input walks every sorted `.yaml`/`.yml` file - the rendered
Deployment and Service - through the documented Kubernetes subset, and
reads `campaign.toml` (the same shape a Compose file puts under
`x-theseus.campaign`) for the declared campaign.

## 4. Run the plan lock

```sh
theseus compose plan rendered > plan.json
grep -n '"chooser"\|calculate' plan.json | head -6
```

The plan names the rendered directory as its input, keeps both Kubernetes
documents' workloads, and locks the declared HTTP operation. To explore
this campaign on a KVM host, follow tutorial 15's conversion flow with
`theseus compose explore --output campaign rendered`.

## 5. Inspect the locked plan

```sh
grep -n 'format' plan.json | head -2
grep -c 'theseus' plan.json
```

Every record the plan locks - services, networks, the campaign policy -
comes from the rendered directory and the two TOML declarations, so the
same input replays byte-stably on any KVM host.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work rendered plan.json
```
