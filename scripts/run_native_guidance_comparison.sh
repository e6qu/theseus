#!/bin/sh
# Retain the side-by-side guidance comparison for the tutorial 30 workload
# on native KVM: explore the same Compose campaign under every guidance
# mode at one fixed budget inside the published runtime, then emit the
# committed comparison artifact from the retained results on the host.
#
# The campaigns are deterministic: rerunning this script on the same
# architecture, host kernel, and runtime digest reproduces them byte-stably.
set -eu
: "${THESEUS_ARCH:?set native architecture}"
: "${THESEUS_IMAGE:?set published compiler/runtime image}"
case "$THESEUS_ARCH" in amd64|arm64) ;; *) echo 'unsupported architecture' >&2; exit 2 ;; esac
budget=${THESEUS_GUIDANCE_BUDGET:-8}
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tutorial="$root/docs/tutorials/30-multiservice-lost-update"
comparison_budget=8

for service in counter worker; do
  mkdir -p "$tutorial/$service/work/runtime" "$tutorial/$service/work/guest"
  docker build --load --platform "linux/$THESEUS_ARCH" \
    -t "theseus-native-$service" "$tutorial/$service"
  docker save "theseus-native-$service" -o "$tutorial/$service/work/$service.tar"
done

# Inside the published runtime: one locked plan, then one campaign per
# guidance mode at the fixed budget. Resumable: retained campaigns are kept.
commands='
  theseus compose plan > plan.json
  for mode in unified coverage adaptive posterior property; do
    if test -d "guidance/$mode"; then
      echo "keeping guidance/$mode"
      continue
    fi
    theseus compose explore \
      --max-runs '"$comparison_budget"' \
      --guidance "$mode" \
      --output "guidance/$mode" compose.yaml
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
    '
  ownership='
    for output in plan.json guidance; do
      if test -e "$output"; then
        chown -R "$HOST_UID:$HOST_GID" "$output"
      fi
    done
  '
  docker run --rm --privileged --platform "linux/$THESEUS_ARCH" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -v "$tutorial":/tutorial -w /tutorial "$THESEUS_IMAGE" sh -ec "$commands$ownership"
fi

# The comparison reads the retained results and needs no KVM, so it runs on
# the host with the freshly built CLI (CI) or an installed theseus.
if test -n "${THESEUS_SOURCE_RUNTIME:-}" && test -x "$THESEUS_SOURCE_RUNTIME/theseus"; then
  theseus="$THESEUS_SOURCE_RUNTIME/theseus"
elif command -v theseus >/dev/null 2>&1; then
  theseus=theseus
else
  echo "theseus not available; run 'theseus evaluate compare' on guidance/* to emit the comparison" >&2
  exit 0
fi
if true; then
  paths=""
  for mode in unified coverage adaptive posterior property; do
    paths="$paths $tutorial/guidance/$mode"
  done
  # shellcheck disable=SC2086
  "$theseus" evaluate compare $paths --format json \
    > "$tutorial/guidance/comparison.json"
  # shellcheck disable=SC2086
  "$theseus" evaluate compare $paths --format markdown \
    > "$tutorial/guidance/comparison.md"
  echo "guidance comparison: $tutorial/guidance/comparison.md"
fi
