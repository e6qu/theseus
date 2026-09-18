# Tutorial 35: Control pthread synchronization

Run a C program whose workers block on a mutex and a condition variable. Let
Theseus schedule only runnable threads, then inspect the retained waits,
wakeups, lock acquisitions, and releases. The program does not use the
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

## 1. Build the service

```sh
sed -n '1,200p' service/main.c
mkdir -p service/work
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  /opt/theseus/instrumentation/c/theseus-schedule-cc \
  --process workers --module condition -o service/work/workers service/main.c
docker run --rm --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" \
  gcc -O2 -o service/work/ready service/ready.c
docker build --load --platform "linux/$THESEUS_ARCH" \
  -t theseus-pthread-sync-tutorial service
docker save theseus-pthread-sync-tutorial -o service/work/service.tar
```

The final image contains only the two compiled commands, the schedule catalog,
and their runtime libraries. This keeps deterministic guest startup short.

Two readers wait until a writer changes `value`. The writer broadcasts the
condition. Every thread uses the same mutex. The compiler frontend wraps
`pthread_mutex_lock`, `pthread_mutex_unlock`,
`pthread_cond_wait`, `pthread_cond_signal`, `pthread_cond_broadcast`, and
`pthread_join`. A blocked worker leaves the runnable set instead of blocking
the selected schedule on the host kernel.

## 2. Enter the published runtime

```sh
docker run --rm -it --privileged --platform "linux/$THESEUS_ARCH" \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh
```

Run steps 3–5 inside this shell.

## 3. Prepare the runtime

```sh
mkdir -p service/work/runtime service/work/guest
cp /usr/local/bin/firecracker service/work/runtime/firecracker
cp /usr/local/bin/theseus-image service/work/runtime/theseus-image
cp /opt/theseus/vmlinux service/work/guest/vmlinux
theseus compose plan > plan.json
grep -n 'runnable_prefixes\|max_variants' plan.json
```

## 4. Run the exploration

Theseus captures a checkpoint at service readiness. Each case and replay
inherits that boot state and checks resumed execution, not repeatable boot.

```sh
theseus compose explore --output campaign compose.yaml
```

## 5. Inspect and replay synchronization

```sh
grep -n 'thread_synchronization_events\|thread_synchronization' \
  campaign/campaign-result.json
theseus report --format markdown --output report/report.md campaign
grep -n 'synchronization events\|sync #' report/report.md
```

The result uses stable first-use numbers such as `mutex-0` and
`condition-1`; it never records ASLR-dependent pthread object addresses.
Each event names its thread and operation. A signal also names the chosen
waiter. A broadcast makes every waiter runnable, after which the recorded
schedule chooses who resumes.

```sh
theseus compose replay campaign --output rerun
theseus compose verify campaign
theseus compose verify rerun
grep -R '"value":42' rerun/services/workers/serial.log
grep -n 'replay_verification' rerun/campaign-result.json
```

Replay rejects a changed scheduling decision or synchronization event.

Keep the entire `campaign` directory to replay it elsewhere. `compose verify`
checks retained inputs, checkpoint ancestry, and complete execution hashes
offline; it does not certify native execution.

## 6. Clean up (optional)

```sh
exit
rm -rf service/work plan.json campaign report rerun
```

This bounded GCC C path supports default mutex locking, untimed condition
waits, signals, broadcasts, and joins. Timed waits, cancellation, semaphores,
direct futex use, blocking I/O, processes, and uninstrumented library
concurrency remain outside this scheduling profile.
