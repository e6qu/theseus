#!/bin/sh
set -eu

: "${THESEUS_ARCH:?set THESEUS_ARCH to amd64 or arm64}"
: "${THESEUS_IMAGE:?set THESEUS_IMAGE to the architecture-specific released image}"
: "${THESEUS_TAG:?set THESEUS_TAG to the 12-character release tag}"

case "$THESEUS_ARCH" in
  amd64|arm64) ;;
  *) echo "unsupported architecture: $THESEUS_ARCH" >&2; exit 2 ;;
esac

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
validation="$root/.native-evidence/$THESEUS_ARCH/validation"
rm -rf "$validation"
mkdir -p "$validation"

runtime() {
  tutorial=$1
  shift
  docker run --rm --privileged --platform "linux/$THESEUS_ARCH" \
    -v "$tutorial":/tutorial -w /tutorial "$THESEUS_IMAGE" sh -ec "$*"
}

prepare_runtime() {
  tutorial=$1
  service=$2
  mkdir -p "$tutorial/$service/work/runtime" "$tutorial/$service/work/guest"
  docker run --rm --platform "linux/$THESEUS_ARCH" \
    -v "$tutorial":/tutorial -w /tutorial "$THESEUS_IMAGE" sh -ec \
    "cp /usr/local/bin/firecracker '$service/work/runtime/firecracker';
     cp /usr/local/bin/theseus-image '$service/work/runtime/theseus-image';
     cp /opt/theseus/vmlinux '$service/work/guest/vmlinux'"
}

container="$root/docs/tutorials/14-container-image"
mkdir -p "$container/work"
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-validation-container "$container"
docker save theseus-validation-container -o "$container/work/service.tar"
prepare_runtime "$container" .
runtime "$container" '
  theseus test --dry-run > plan.json
  theseus test --output work/replay theseus.toml
  grep -a "^THES:HTTP:operation:read_health:PASS$" work/replay/serial.log
  theseus replay work/replay > replay.log
  grep -F "replay passed" replay.log
'
mkdir -p "$validation/container"
cp "$container/plan.json" "$container/replay.log" "$validation/container/"
cp -a "$container/work/replay" "$validation/container/run"
mkdir -p "$validation/container/source"
cp "$container/Dockerfile" "$container/theseus.toml" "$validation/container/source/"

coverage="$root/docs/tutorials/31-c-basic-block-coverage"
mkdir -p "$coverage/service/work"
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$coverage":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/c/theseus-coverage-cc \
  --process classifier --module branching -o service/work/classify service/main.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$coverage":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -o service/work/ready service/ready.c
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-validation-coverage "$coverage/service"
docker save theseus-validation-coverage -o "$coverage/service/work/service.tar"
prepare_runtime "$coverage" service
runtime "$coverage" '
  theseus compose plan > plan.json
  theseus compose explore --output campaign compose.yaml
  grep -E "\"unique_application_blocks\": [1-9][0-9]*" campaign/campaign-result.json
  theseus report --format markdown --output report/report.md campaign
  theseus compose replay campaign --output rerun
  grep -A2 "\"replay_verification\"" rerun/campaign-result.json | grep "\"status\": \"passed\""
  theseus compare campaign rerun > comparison.json
  grep -F "\"format\": \"theseus-campaign-comparison-v1\"" comparison.json
  theseus evaluate capture campaign --output evaluation --name "C coverage campaign"
  theseus evaluate evaluation/theseus-evaluation.toml > evaluation.json
  grep -F "\"status\": \"passed\"" evaluation.json
'
mkdir -p "$validation/coverage"
cp "$coverage/plan.json" "$coverage/comparison.json" "$coverage/evaluation.json" \
  "$validation/coverage/"
cp -a "$coverage/campaign" "$coverage/report" "$coverage/rerun" \
  "$coverage/evaluation" "$validation/coverage/"
mkdir -p "$validation/coverage/source/service"
cp "$coverage/compose.yaml" "$validation/coverage/source/"
cp "$coverage/service/Dockerfile" "$coverage/service/main.c" \
  "$coverage/service/ready.c" "$coverage/service/theseus.toml" \
  "$validation/coverage/source/service/"

schedule="$root/docs/tutorials/33-search-thread-schedules"
mkdir -p "$schedule/service/work"
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$schedule":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/c/theseus-schedule-cc \
  --process ledger --module deposit -o service/work/ledger service/main.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$schedule":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -o service/work/ready service/ready.c
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-validation-schedule "$schedule/service"
docker save theseus-validation-schedule -o "$schedule/service/work/service.tar"
prepare_runtime "$schedule" service
runtime "$schedule" '
  theseus compose plan > plan.json
  theseus compose explore --expect-counterexample lost_update_is_unreachable \
    --output campaign compose.yaml
  grep -E "\"thread_scheduling_decisions\": [1-9][0-9]*" campaign/campaign-result.json
  theseus report --format markdown --output report/report.md campaign
  theseus compose explore --minimize campaign \
    --expect-counterexample lost_update_is_unreachable --output minimized
  theseus compose replay minimized --output rerun
  grep -A2 "\"replay_verification\"" rerun/campaign-result.json | grep "\"status\": \"passed\""
'
mkdir -p "$validation/schedule-search"
cp "$schedule/plan.json" "$validation/schedule-search/"
cp -a "$schedule/campaign" "$schedule/report" "$schedule/minimized" \
  "$schedule/rerun" "$validation/schedule-search/"
mkdir -p "$validation/schedule-search/source/service"
cp "$schedule/compose.yaml" "$validation/schedule-search/source/"
cp "$schedule/service/Dockerfile" "$schedule/service/main.c" \
  "$schedule/service/ready.c" "$schedule/service/theseus.toml" \
  "$validation/schedule-search/source/service/"

pthread="$root/docs/tutorials/35-control-pthread-synchronization"
mkdir -p "$pthread/service/work"
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$pthread":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/c/theseus-schedule-cc \
  --process workers --module condition -o service/work/workers service/main.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$pthread":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -o service/work/ready service/ready.c
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-validation-pthread "$pthread/service"
docker save theseus-validation-pthread -o "$pthread/service/work/service.tar"
prepare_runtime "$pthread" service
runtime "$pthread" '
  theseus compose plan > plan.json
  theseus compose explore --output campaign compose.yaml
  grep -E "\"thread_synchronization_events\": [1-9][0-9]*" campaign/campaign-result.json
  theseus report --format markdown --output report/report.md campaign
  theseus compose replay campaign --output rerun
  grep -A2 "\"replay_verification\"" rerun/campaign-result.json | grep "\"status\": \"passed\""
'
mkdir -p "$validation/pthread-sync"
cp "$pthread/plan.json" "$validation/pthread-sync/"
cp -a "$pthread/campaign" "$pthread/report" "$pthread/rerun" "$validation/pthread-sync/"
mkdir -p "$validation/pthread-sync/source/service"
cp "$pthread/compose.yaml" "$validation/pthread-sync/source/"
cp "$pthread/service/Dockerfile" "$pthread/service/main.c" \
  "$pthread/service/ready.c" "$pthread/service/theseus.toml" \
  "$validation/pthread-sync/source/service/"

execution="$root/docs/tutorials/41-reject-execution-divergence"
mkdir -p "$execution/api/work"
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-validation-strict-execution "$execution"
docker save theseus-validation-strict-execution -o "$execution/api/work/service.tar"
prepare_runtime "$execution" api
runtime "$execution" '
  theseus compose plan > plan.json
  theseus compose explore --output campaign compose.yaml
  grep -E "\"execution_decisions\": [1-9][0-9]*" campaign/campaign-result.json
  grep -E "\"sha256\": \"[0-9a-f]{64}\"" campaign/campaign-result.json
  grep -F '"host:serial_input:' campaign/campaign-result.json
  theseus report --format markdown --output report/report.md campaign
  grep -F "Execution ledger" report/report.md
  theseus compose replay campaign --output rerun
  grep -A2 "\"replay_verification\"" rerun/campaign-result.json | grep "\"status\": \"passed\""
  theseus compare campaign rerun > comparison.json
  grep -F "\"status\": \"same\"" comparison.json
'
mkdir -p "$validation/strict-execution"
cp "$execution/plan.json" "$execution/comparison.json" \
  "$validation/strict-execution/"
cp -a "$execution/campaign" "$execution/report" "$execution/rerun" \
  "$validation/strict-execution/"
mkdir -p "$validation/strict-execution/source/api"
cp "$execution/.dockerignore" "$execution/Dockerfile" "$execution/compose.yaml" \
  "$validation/strict-execution/source/"
cp "$execution/api/theseus.toml" \
  "$validation/strict-execution/source/api/"

source_commit=$(git -C "$root" rev-parse HEAD)
image_digest=$(docker image inspect "$THESEUS_IMAGE" --format '{{index .RepoDigests 0}}')
kvm_api=$(python3 -c 'import fcntl, os; fd = os.open("/dev/kvm", os.O_RDWR); print(fcntl.ioctl(fd, 0xAE00, 0))')
python3 "$root/scripts/runtime_validation_evidence.py" \
  --root "$validation" \
  --architecture "$THESEUS_ARCH" \
  --source-commit "$source_commit" \
  --runtime-image "$image_digest" \
  --runtime-tag "$THESEUS_TAG-$THESEUS_ARCH" \
  --host-kernel "$(uname -r)" \
  --kvm-api-version "$kvm_api"

source_date_epoch=$(git -C "$root" show -s --format=%ct "$source_commit")
python3 "$root/scripts/reproducible_tar.py" \
  --mtime "$source_date_epoch" \
  --output "$root/.native-evidence/$THESEUS_ARCH/theseus-${THESEUS_TAG}-runtime-validation-${THESEUS_ARCH}.tar.gz" \
  "$validation"
