#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
compiler="$root/instrumentation/c/theseus-coverage-cc"
source="$root/scripts/tests/c_coverage_fixture.c"

"$compiler" --process fixture --module classifier -o "$work/fixture" "$source"
"$compiler" --process fixture --module classifier -o "$work/rebuilt" "$source"

first=$("$work/fixture" 0 2>&1 || true)
second=$("$work/fixture" 7 2>&1 || true)
records=$(printf '%s\n%s\n' "$first" "$second" | sort -u)
build_sha256=$(sed -n 's/.*"build_sha256": "\([0-9a-f]*\)".*/\1/p' \
    "$work/fixture.theseus-coverage.json")
rebuilt_sha256=$(sed -n 's/.*"build_sha256": "\([0-9a-f]*\)".*/\1/p' \
    "$work/rebuilt.theseus-coverage.json")

[ "${#build_sha256}" -eq 64 ]
[ "$build_sha256" = "$rebuilt_sha256" ]
printf '%s\n' "$records" | grep -E \
    "^THES:COV:v1:fixture:classifier:${build_sha256}:0x[0-9a-f]+$" >/dev/null
[ "$(printf '%s\n' "$records" | grep -c '^THES:COV:v1:')" -ge 3 ]
grep -F '"format": "theseus-c-coverage-build-v1"' \
    "$work/fixture.theseus-coverage.json" >/dev/null
grep -E '"preprocessed_sha256": "[0-9a-f]{64}"' \
    "$work/fixture.theseus-coverage.json" >/dev/null
