# Tutorial 32: Reproduce a C thread race

Compile a pthread program with the scheduler shipped in the Theseus runtime.
Run one sequential schedule and one interleaving that loses an update. Then
inspect and replay every scheduling decision. The program does not link the
Theseus SDK.

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

## 1. Build and inspect the race

```sh
sed -n '1,160p' service/main.c
```

Two threads load the same atomic balance and store their own result. The
individual accesses are valid C, but the read-modify-write transaction is not
atomic.

```sh
mkdir -p service/work
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/c/theseus-schedule-cc \
  --process ledger --module deposit -o service/work/ledger service/main.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -o service/work/ready service/ready.c
sed -n '1,120p' service/work/ledger.theseus-schedule.json
```

The frontend inserts a scheduling point at each application basic block. It
assigns thread `0` to `main` and assigns later identities in `pthread_create`
order. The manifest binds every decision to this exact build.

Build the service image and inspect the two schedules:

```sh
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-thread-scheduling-tutorial service
docker save theseus-thread-scheduling-tutorial -o service/work/service.tar
sed -n '1,180p' compose.yaml
```

`0,1,2` completes the deposits sequentially. `0,0,0,1,2` lets both workers
load before either stores and produces the lost update.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–5 inside this shell.

## 3. Prepare the locked runtime inputs

```sh
mkdir -p service/work/runtime service/work/guest
cp /usr/local/bin/firecracker service/work/runtime/firecracker
cp /usr/local/bin/theseus-image service/work/runtime/theseus-image
cp /opt/theseus/vmlinux service/work/guest/vmlinux
theseus compose plan > plan.json
grep -n 'thread_schedule\|THESEUS_THREAD_SCHEDULE' plan.json
```

## 4. Run the campaign

```sh
theseus compose explore --output campaign compose.yaml
```

## 5. Inspect and replay the lost update

```sh
grep -n 'thread_scheduling_decisions\|lost_update_is_reachable' \
  campaign/campaign-result.json
grep -R '^THES:SCHED:v1:ledger:deposit:' \
  campaign/runs/*/services/ledger/serial.log | head
theseus report campaign --output report
grep -n 'Thread scheduling\|Scheduling decisions' report/report.md
```

Each scheduling record names the decision number, current thread, runnable
thread mask, selected thread, and module-relative scheduling point. The report
puts the decisions beside the operation that produced them.

```sh
theseus compose replay campaign --output rerun
grep -n '"status": "passed"\|thread_scheduling_decisions' \
  rerun/campaign-result.json
```

Replay fails if the runnable sets, selected threads, decision order, or
build-scoped scheduling points change.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work plan.json campaign report rerun
```

This first scheduler is deliberately bounded: 32 threads, 8,192 decisions,
GCC C programs and application basic-block boundaries. The frontend controls
joins, default mutex locking, and untimed condition waits, signals, and
broadcasts. Tutorial 35 exercises those synchronization operations.
