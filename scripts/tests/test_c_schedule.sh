#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
compiler="$root/instrumentation/c/theseus-schedule-cc"
source="$root/scripts/tests/c_schedule_fixture.c"
sync_source="$root/scripts/tests/c_schedule_sync_fixture.c"

"$compiler" --process fixture --module lost_update -o "$work/fixture" "$source"
"$compiler" --process fixture --module lost_update -o "$work/rebuilt" "$source"
if "$compiler" --process fixture --module 'invalid:name' \
    -o "$work/invalid" "$source" >/dev/null 2>&1; then
    echo 'scheduler frontend accepted a record-delimiter character' >&2
    exit 1
fi
build_sha256=$(sed -n 's/.*"build_sha256": "\([0-9a-f]*\)".*/\1/p' \
    "$work/fixture.theseus-schedule.json")
rebuilt_sha256=$(sed -n 's/.*"build_sha256": "\([0-9a-f]*\)".*/\1/p' \
    "$work/rebuilt.theseus-schedule.json")

[ "${#build_sha256}" -eq 64 ]
[ "$build_sha256" = "$rebuilt_sha256" ]

for example in '0,1,2 balance=42' '0,0,0,1,2 balance=22'; do
    schedule=${example%% *}
    expected=${example#* }
    first=$(THESEUS_THREAD_SCHEDULE=$schedule "$work/fixture" 2>&1)
    attempt=0
    while [ "$attempt" -lt 20 ]; do
        repeated=$(THESEUS_THREAD_SCHEDULE=$schedule "$work/fixture" 2>&1)
        [ "$first" = "$repeated" ]
        attempt=$((attempt + 1))
    done
    printf '%s\n' "$first" | grep -F "$expected" >/dev/null
    printf '%s\n' "$first" | grep -E \
        "^THES:SCHED:v1:fixture:lost_update:${build_sha256}:[0-9]+:[0-9]+:0x[0-9a-f]{8}:[0-9]+:0x[0-9a-f]+$" >/dev/null
done

# Runnable-prefix mode consumes choices only when the retained mask contains
# more than one runnable thread. An empty prefix is the deterministic root;
# this derived prefix reaches the fixture's lost update.
root_run=$(THESEUS_THREAD_SCHEDULE_MODE=runnable_prefix \
    THESEUS_THREAD_SCHEDULE= "$work/fixture" 2>&1)
root_repeat=$(THESEUS_THREAD_SCHEDULE_MODE=runnable_prefix \
    THESEUS_THREAD_SCHEDULE= "$work/fixture" 2>&1)
[ "$root_run" = "$root_repeat" ]
printf '%s\n' "$root_run" | grep -F 'balance=42' >/dev/null

failing=$(THESEUS_THREAD_SCHEDULE_MODE=runnable_prefix \
    THESEUS_THREAD_SCHEDULE=0,0,1,1,2,2,2,1 "$work/fixture" 2>&1)
printf '%s\n' "$failing" | grep -F 'balance=22' >/dev/null

if THESEUS_THREAD_SCHEDULE_MODE=runnable_prefix \
    THESEUS_THREAD_SCHEDULE=31 "$work/fixture" >/dev/null 2>&1; then
    echo 'runnable-prefix mode accepted a thread outside the observed mask' >&2
    exit 1
fi
grep -F '"format": "theseus-c-schedule-build-v1"' \
    "$work/fixture.theseus-schedule.json" >/dev/null
grep -E '"preprocessed_sha256": "[0-9a-f]{64}"' \
    "$work/fixture.theseus-schedule.json" >/dev/null

"$compiler" --process fixture --module synchronization \
    -o "$work/synchronization" "$sync_source"
sync_sha256=$(sed -n 's/.*"build_sha256": "\([0-9a-f]*\)".*/\1/p' \
    "$work/synchronization.theseus-schedule.json")
sync_first=$(THESEUS_THREAD_SCHEDULE_MODE=runnable_prefix \
    THESEUS_THREAD_SCHEDULE= "$work/synchronization" 2>&1)
attempt=0
while [ "$attempt" -lt 20 ]; do
    sync_repeat=$(THESEUS_THREAD_SCHEDULE_MODE=runnable_prefix \
        THESEUS_THREAD_SCHEDULE= "$work/synchronization" 2>&1)
    [ "$sync_first" = "$sync_repeat" ]
    attempt=$((attempt + 1))
done
printf '%s\n' "$sync_first" | grep -F 'value=42' >/dev/null
for operation in wait signal broadcast resume acquire release-for-wait; do
    printf '%s\n' "$sync_first" | grep -E \
        "^THES:SYNC:v1:fixture:synchronization:${sync_sha256}:[0-9]+:[0-9]+:${operation}:(mutex|condition):[0-9]+:(-|[0-9]+)$" >/dev/null
done
contended=$(THESEUS_THREAD_SCHEDULE_MODE=runnable_prefix \
    THESEUS_THREAD_SCHEDULE=0,0,2,2,1 "$work/synchronization" 2>&1)
attempt=0
while [ "$attempt" -lt 20 ]; do
    contended_repeat=$(THESEUS_THREAD_SCHEDULE_MODE=runnable_prefix \
        THESEUS_THREAD_SCHEDULE=0,0,2,2,1 "$work/synchronization" 2>&1)
    [ "$contended" = "$contended_repeat" ]
    attempt=$((attempt + 1))
done
printf '%s\n' "$contended" | grep -E \
    "^THES:SYNC:v1:fixture:synchronization:${sync_sha256}:[0-9]+:1:wait:mutex:[0-9]+:-$" >/dev/null
grep -F '"controlled_pthread_operations": ["mutex_lock", "mutex_unlock", "cond_wait", "cond_signal", "cond_broadcast", "join"]' \
    "$work/synchronization.theseus-schedule.json" >/dev/null
grep -F '"maximum_synchronization_events": 8192' \
    "$work/synchronization.theseus-schedule.json" >/dev/null
grep -F '"maximum_synchronization_objects": 128' \
    "$work/synchronization.theseus-schedule.json" >/dev/null
