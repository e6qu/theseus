# Tutorial 34: Explore runnable thread choices

Build a pthread program with the Theseus scheduler. Let Theseus run the
program, observe which threads are runnable, and fork new schedules only at
those choices. The search finds a lost update without a handwritten schedule
or the Theseus SDK.

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

## 1. Build the program

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
  -t theseus-runnable-prefix-tutorial service
docker save theseus-runnable-prefix-tutorial -o service/work/service.tar
```

Both workers load the balance and then store their own update. Each atomic
access is valid, but the pair is not one atomic transaction.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–5 inside this shell.

## 3. Prepare and inspect the exploration bound

```sh
mkdir -p service/work/runtime service/work/guest
cp /usr/local/bin/firecracker service/work/runtime/firecracker
cp /usr/local/bin/theseus-image service/work/runtime/theseus-image
cp /opt/theseus/vmlinux service/work/guest/vmlinux
theseus compose plan > plan.json
grep -n 'thread_schedule_exploration\|runnable_prefixes\|max_choices' plan.json
```

`max_choices` limits the observed choice depth. `max_variants` limits the
schedule tree. The plan contains one initial command, not a precomputed list
of thread sequences.

## 4. Run the exploration

```sh
theseus compose explore --expect-counterexample lost_update_is_unreachable \
  --output campaign compose.yaml
grep -n 'thread_schedule_prefixes\|lost_update_is_unreachable' \
  campaign/campaign-result.json
```

The first run chooses the lowest runnable thread after its prefix ends. Each
later run changes one choice that the previous execution proved runnable. A
counterexample reports balance `22`: both workers loaded zero before either
worker stored its update.

## 5. Inspect, minimize, and replay it

```sh
theseus report campaign --output report
grep -n 'Runnable prefix\|Scheduling decisions' report/report.md
theseus compose explore --minimize campaign \
  --expect-counterexample lost_update_is_unreachable --output minimized
grep -n 'thread_schedule_prefixes\|THESEUS_THREAD_SCHEDULE' \
  minimized/minimization.json minimized/replay-plan.json
theseus compose replay minimized --output rerun
grep -R '"balance":22' rerun/services/ledger/serial.log
```

The result records the chosen prefix and every observed runnable mask. Replay
uses the recorded prefix and rejects a changed scheduling trace.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work plan.json campaign report minimized rerun
```

This scheduler controls instrumented GCC C basic blocks. It supports at most
32 pthread identities and 8,192 decisions. Joins, default mutex locking, and
untimed condition waits, signals, and broadcasts update the runnable set.
