# Tutorial 22: Supply an image secret with Compose

`worker` is a normal BusyBox HTTP image. `compose.yaml` replaces its default
`/www/token` with `worker/secret/token`. Theseus locks those bytes as a
root-only file in the worker initramfs. `api` reads the token, then replays the
same campaign. Treat the campaign and replay directories as sensitive: they
contain the locked secret.

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
name=theseus-compose-secret
mkdir -p api/work worker/work
docker build --platform linux/arm64 -t "$name-api" api
docker build --platform linux/arm64 -t "$name-worker" worker
docker save "$name-api" -o api/work/api.tar
docker save "$name-worker" -o worker/work/worker.tar
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
for service in api worker; do
  mkdir -p "$service/work/runtime" "$service/work/guest"
  cp /usr/local/bin/firecracker "$service/work/runtime/firecracker"
  cp /usr/local/bin/theseus-image "$service/work/runtime/theseus-image"
  cp /opt/theseus/vmlinux "$service/work/guest/vmlinux"
done
```

These copies become inputs to the service manifests. Planning hashes them;
replay does not silently select newer binaries from the container.

## 4. Run, inspect, and replay

```sh
theseus compose explore --output campaign compose.yaml
grep -a '^THES:SHELL:operation:read_secret:PASS$' campaign/services/api/serial.log
theseus compose replay campaign --output rerun
grep -a '^THES:SHELL:operation:read_secret:PASS$' rerun/services/api/serial.log
echo 'PASS: Theseus locked a Compose secret into an image service'
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
rm -rf api/work worker/work campaign rerun
```
