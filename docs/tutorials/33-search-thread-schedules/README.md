# Tutorial 33: Search C thread schedules

Compile a pthread program with the Theseus scheduler. Ask Theseus to enumerate
a bounded set of thread schedules, find a lost update, minimize the failing
timeline, and replay it. The program does not link the Theseus SDK.

## Before you start

Use a Linux host with KVM and Docker. Run every command from this directory.
Select a published 12-character Theseus release SHA:

```sh
export THESEUS_TAG=<12-character-sha>
case "$(uname -m)" in
  x86_64) export THESEUS_ARCH=amd64 ;;
  aarch64|arm64) export THESEUS_ARCH=arm64 ;;
  *) echo 'This tutorial requires amd64 or arm64 Linux' >&2; exit 1 ;;
esac
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-${THESEUS_ARCH}
```

## 1. Build the race

```sh
sed -n '1,160p' service/main.c
mkdir -p service/work
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/c/theseus-schedule-cc \
  --process ledger --module deposit -o service/work/ledger service/main.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -o service/work/ready service/ready.c
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-thread-search-tutorial service
docker save theseus-thread-search-tutorial -o service/work/service.tar
```

Each worker loads an atomic balance and later stores its own update. The
individual accesses are valid C, but the transaction is not atomic.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–5 inside this shell.

## 3. Prepare and inspect the search

```sh
mkdir -p service/work/runtime service/work/guest
cp /usr/local/bin/firecracker service/work/runtime/firecracker
cp /usr/local/bin/theseus-image service/work/runtime/theseus-image
cp /opt/theseus/vmlinux service/work/guest/vmlinux
theseus compose plan > plan.json
grep -n 'thread_schedule_search\|generated_schedules\|schedule-0-0-0-1-2' plan.json
```

The Compose file names threads `0`, `1`, and `2`, sets a five-choice repeating
period, and permits at most three switches around that period. Theseus expands
that contract into 123 schedules. Every generated case and its exact choice
sequence are stored in `plan.json`; replay never regenerates the search space.

## 4. Run the search

```sh
theseus compose explore --expect-counterexample lost_update_is_unreachable \
  --output campaign compose.yaml
grep -n 'lost_update_is_unreachable\|schedule-0-0-0-1-2' \
  campaign/campaign-result.json
```

The counterexample reports balance `22`: both workers loaded zero before
either worker stored its update.

## 5. Inspect, minimize, and replay the failure

```sh
theseus report campaign --output report
grep -n 'search 123 pattern\|Scheduling decisions' report/report.md
theseus compose explore --minimize campaign \
  --expect-counterexample lost_update_is_unreachable --output minimized
grep -n 'schedule-\|THESEUS_THREAD_SCHEDULE' minimized/replay-plan.json
theseus compose replay minimized --output rerun
grep -R '"balance":22' rerun/services/ledger/serial.log
```

The minimized bundle keeps the failing schedule case. Replay also checks the
ordered runnable masks, selected threads, build identity, and scheduling-point
offsets recorded by that case.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work plan.json campaign report minimized rerun
```

This search enumerates periodic patterns declared before execution. It does
not yet derive new prefixes from observed runnable sets. The scheduler remains
bounded to GCC C, 32 pthread identities, 8,192 decisions, and `pthread_join` as
its only modeled blocking operation.
