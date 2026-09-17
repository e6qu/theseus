# Tutorial 26: Set Compose host identity

`api` is a normal BusyBox HTTP image. `compose.yaml` gives it the hostname
`api.local` and a fixed `cache.local` entry. Theseus writes both into the
locked image runtime, checks them inside the guest, then replays the campaign.

Use literal DNS names and IP addresses. Theseus never asks a host resolver for
an `extra_hosts` address.

## Before you start

Use a Linux host with KVM and Docker. Run every host command from this
tutorial directory. Choose a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

The local Dockerfile, Compose or manifest files, and input files below this
directory are the complete tutorial input. No Theseus checkout is used.

## 1. Build the service images

Run these commands on the host:

```sh
name=theseus-compose-host-identity
mkdir -p api/work
docker build --platform linux/arm64 -t "$name-api" api
docker save "$name-api" -o api/work/api.tar
```

Inspect the saved image inputs before continuing:

```sh
find . -path '*/work/*.tar' -type f -print
```

You should see one archive for each service image used by this tutorial.

## 2. Enter the published runtime

Start an interactive shell with this directory mounted as `/tutorial`:

```sh
docker run --rm -it --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

The remaining commands in steps 3–5 run inside that shell.

## 3. Prepare the locked runtime inputs

```sh
mkdir -p api/work/runtime api/work/guest
cp /usr/local/bin/firecracker api/work/runtime/firecracker
cp /usr/local/bin/theseus-image api/work/runtime/theseus-image
cp /opt/theseus/vmlinux api/work/guest/vmlinux
```

These copies become inputs to the service manifests. Planning hashes them;
replay does not silently select newer binaries from the container.

## 4. Run, inspect, and replay

```sh
theseus compose explore --output campaign compose.yaml
grep -aF 'THES:SHELL:operation:inspect_host_identity:PASS' campaign/services/api/serial.log
theseus compose replay campaign --output rerun
grep -aF 'THES:SHELL:operation:inspect_host_identity:PASS' rerun/services/api/serial.log
echo 'PASS: Theseus locked Compose hostname and extra_hosts into an image service'
```

Each `grep` is an observation to review. It exits successfully only when
the recorded bundle contains the behavior named by that command. The replay
command consumes the recorded bundle rather than selecting new artifacts.

## 5. Inspect the retained evidence

```sh
find . \( -path '*/replay-plan.json' -o -path '*/campaign-result.json' \) -type f
find . \( -path '*/result.json' -o -path '*/serial.log' \) -type f
exit
```

Keep `campaign/` and `rerun/` when investigating a result.

## 6. Clean up (optional)

Back on the host, remove generated data only after inspection:

```sh
rm -rf api/work campaign rerun
```
