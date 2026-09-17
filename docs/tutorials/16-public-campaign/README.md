# Tutorial 16: Publish a public campaign

The tutorial runs a normal BusyBox HTTP image as a Compose campaign, then
publishes its completed replay directory:

```sh
theseus evaluate capture campaign --output public-evaluation --name "HTTP health campaign"
```

`public-evaluation/` contains the copied campaign, a version 2 evaluation
contract, and a lock covering every copied file. Give that directory to a
reader with the published `theseus` binary. They can inspect the result and
replay the campaign without this source tree or the original campaign output.

Use capture only after the campaign is complete. It records the observed
status and property outcomes; add an independently repeatable conventional
baseline later if you want one.

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
name=theseus-public-campaign-tutorial
mkdir -p api/work
docker build --platform linux/arm64 -t "$name" .
docker save "$name" -o api/work/service.tar
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
theseus evaluate capture campaign --output public-evaluation --name "HTTP health campaign"
theseus evaluate public-evaluation/theseus-evaluation.toml > evaluation.json
grep -Fq '"status": "passed"' evaluation.json
grep -Fq '"files":' evaluation.json
theseus compose replay public-evaluation/campaign --output rerun
grep -aF 'THES:HTTP:operation:read_health:PASS' rerun/services/api/serial.log
echo 'PASS: Theseus published and replayed a locked public campaign'
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
rm -rf api/work campaign public-evaluation rerun evaluation.json
```
