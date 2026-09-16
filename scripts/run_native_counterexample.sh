#!/bin/sh
# Reproduce the tutorial's ordinary three-service partition/recovery failure.
set -eu
: "${THESEUS_ARCH:?set native architecture}"
: "${THESEUS_IMAGE:?set published compiler/runtime image}"
case "$THESEUS_ARCH" in amd64|arm64) ;; *) echo 'unsupported architecture' >&2; exit 2 ;; esac
if test -n "${THESEUS_SOURCE_RUNTIME:-}"; then
  case "$THESEUS_SOURCE_RUNTIME" in /*) ;; *) echo 'source runtime must be an absolute directory' >&2; exit 2 ;; esac
  for binary in theseus theseus-topology theseus-image firecracker; do
    test -x "$THESEUS_SOURCE_RUNTIME/$binary"
  done
  test -f "$THESEUS_SOURCE_RUNTIME/vmlinux"
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tutorial="$root/docs/tutorials/30-multiservice-lost-update"
for service in counter worker; do
  mkdir -p "$tutorial/$service/work/runtime" "$tutorial/$service/work/guest"
  docker build --load --platform "linux/$THESEUS_ARCH" \
    -t "theseus-native-$service" "$tutorial/$service"
  docker save "theseus-native-$service" -o "$tutorial/$service/work/$service.tar"
done
commands='
  theseus compose plan > plan.json
  theseus compose explore --expect-counterexample distributed_lost_update_is_unreachable --output campaign compose.yaml
  theseus compose explore --minimize campaign --expect-counterexample distributed_lost_update_is_unreachable --output minimized
  theseus compose replay minimized --output rerun
  grep -R "\"value\":1" campaign/runs/*/services/counter/serial.log
  grep -R "\"value\":2" campaign/runs/*/services/counter/serial.log
  grep -F "backplane:partition@setup" minimized/minimization.json
  grep -F "backplane:heal@probe_partition" minimized/minimization.json
  grep -E "\"dropped\": [1-9][0-9]*" rerun/services/*/result.json
  grep -F "\"network\":\"recovered\"" rerun/services/writer-a/serial.log
  grep -F "\"kind\": \"partition\"" rerun/topology-result.json
  grep -F "\"kind\": \"heal\"" rerun/topology-result.json
  grep -F "\"value\":1" rerun/services/counter/serial.log
  theseus compose verify campaign
  theseus compose verify minimized
  theseus compose verify rerun
  mkdir retained
  cp -a minimized retained/minimized
  theseus compose verify retained/minimized
  theseus compose replay retained/minimized --output retained/rerun
  theseus compose verify retained/rerun
  for service in counter writer-a writer-b; do
    cmp rerun/services/$service/serial.log retained/rerun/services/$service/serial.log
  done
'
if test -n "${THESEUS_SOURCE_RUNTIME:-}"; then
  for service in counter worker; do
    cp "$THESEUS_SOURCE_RUNTIME/firecracker" "$tutorial/$service/work/runtime/firecracker"
    cp "$THESEUS_SOURCE_RUNTIME/theseus-image" "$tutorial/$service/work/runtime/theseus-image"
    cp "$THESEUS_SOURCE_RUNTIME/vmlinux" "$tutorial/$service/work/guest/vmlinux"
  done
  (cd "$tutorial"; PATH="$THESEUS_SOURCE_RUNTIME:$PATH" sh -ec "$commands")
else
  docker run --rm --privileged --platform "linux/$THESEUS_ARCH" \
    -v "$tutorial":/tutorial -w /tutorial "$THESEUS_IMAGE" sh -ec '
      for service in counter worker; do
        cp /usr/local/bin/firecracker "$service/work/runtime/firecracker"
        cp /usr/local/bin/theseus-image "$service/work/runtime/theseus-image"
        cp /opt/theseus/vmlinux "$service/work/guest/vmlinux"
      done
      cp /opt/theseus/pivot.json runtime-pivot.json
    '
  docker run --rm --privileged --platform "linux/$THESEUS_ARCH" \
    -v "$tutorial":/tutorial -w /tutorial "$THESEUS_IMAGE" sh -ec "$commands"
fi
