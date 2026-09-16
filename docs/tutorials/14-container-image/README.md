# Tutorial 14: Run a container image

`Dockerfile` is an ordinary BusyBox HTTP service. `theseus.toml` names its
Docker archive as `guest.image`, then declares a readiness endpoint, an HTTP
operation, and an assertion. Theseus starts the service, waits for readiness,
requests `/health`, records the result, and stops the service.

Replace `Dockerfile` with your own image. Change the URLs and assertions in
`theseus.toml` to match it. Keep the service dependencies in the image; no
guest SDK or Dockerfile instrumentation is needed.

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
work=work
name=theseus-container-image-tutorial
mkdir "$work"
docker build --platform linux/arm64 -t "$name" .
docker save "$name" -o "$work/service.tar"
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
mkdir -p work/runtime work/guest
cp /usr/local/bin/firecracker work/runtime/firecracker
cp /usr/local/bin/theseus-image work/runtime/theseus-image
cp /opt/theseus/vmlinux work/guest/vmlinux
```

These copies become inputs to the service manifests. Planning hashes them;
replay does not silently select newer binaries from the container.

## 4. Run, inspect, and replay

```sh
theseus test --output work/replay theseus.toml
grep -a '^THES:HTTP:operation:read_health:PASS$' work/replay/serial.log
theseus replay --output work/rerun work/replay
cmp work/replay/execution.json work/rerun/execution.json
echo 'PASS: Theseus checked and replayed an unmodified container service'
```

Each `grep` is an observation to review. It exits successfully only when
the recorded bundle contains the behavior named by that command. The replay
command consumes the recorded bundle rather than selecting new artifacts.
For releases with machine-stream capture, replay must admit the exact ordered
device/input stream through guest exit, not just produce the same health line.
Uncontrolled kernel timing can still cause replay to fail; inspect the retained
diagnostics instead of treating a rerun of the seed as deterministic replay.

## 5. Inspect the retained evidence

```sh
find work/replay work/rerun -name 'replay-plan.json' -o -name 'result.json' -o -name 'serial.log' -o -name 'execution.json'
exit
```

Keep `work/replay/` when investigating a result.

## 6. Clean up (optional)

Back on the host, remove generated data only after inspection:

```sh
rm -rf work
```
