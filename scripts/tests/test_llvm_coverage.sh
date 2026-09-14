#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
clang_frontend="$root/instrumentation/llvm/theseus-coverage-clang"
rust_frontend="$root/instrumentation/llvm/theseus-coverage-rustc"
inspect="$root/instrumentation/llvm/theseus-coverage-inspect"

compile_and_check() {
    language=$1
    module=$2
    source=$3
    output=$4
    shift 4
    "$clang_frontend" --process fixture --module "$module" --language "$language" \
        --symbols "$work/symbols" -o "$output" "$source" "$@"
    "$inspect" "$output" "$output.theseus-coverage.json" | \
        grep -E '^instrumented: [1-9][0-9]* LLVM coverage guards$' >/dev/null
    build=$(sed -n 's/.*"build_sha256": "\([0-9a-f]*\)".*/\1/p' \
        "$output.theseus-coverage.json")
    [ "${#build}" -eq 64 ]
    grep -F '"maximum_edges": 65535' "$output.theseus-coverage.json" >/dev/null
    grep -E '"gnu_build_id": "[0-9a-f]+"' "$output.theseus-coverage.json" >/dev/null
    records=$("$output" 0 2>&1 || true; "$output" 7 2>&1 || true)
    printf '%s\n' "$records" | grep -E \
        "^THES:COV:v2:fixture:${module}:${build}:[1-9][0-9]*:0x[0-9a-f]+$" >/dev/null
    [ "$(printf '%s\n' "$records" | grep -c '^THES:COV:v2:')" -ge 2 ]
    test -s "$work/symbols/${module}-${build}.debug"
    offset=$(printf '%s\n' "$records" | awk -F: -v module="$module" \
        '$1 == "THES" && $2 == "COV" && $5 == module { print $8; exit }')
    "$inspect" "$work/symbols/${module}-${build}.debug" \
        "$output.theseus-coverage.json" "$offset" | grep -F "$(basename "$source")" >/dev/null
}

compile_and_check c c "$root/scripts/tests/llvm_coverage_fixture.c" "$work/c-fixture"
compile_and_check c++ cxx "$root/scripts/tests/llvm_coverage_fixture.cc" "$work/cxx-fixture"
"$clang_frontend" --process fixture --module c --language c --symbols "$work/symbols" \
    -o "$work/c-rebuilt" \
    "$root/scripts/tests/llvm_coverage_fixture.c" >/dev/null
cmp "$work/c-fixture.theseus-coverage.json" \
    "$work/c-rebuilt.theseus-coverage.json"
sed '/"gnu_build_id":/d' "$work/c-fixture.theseus-coverage.json" > "$work/legacy.json"
"$inspect" "$work/c-fixture" "$work/legacy.json" >/dev/null
if "$inspect" "$work/c-fixture" "$work/cxx-fixture.theseus-coverage.json" \
    >/dev/null 2>&1; then
    echo 'coverage inspection accepted a manifest from another build' >&2
    exit 1
fi

"$rust_frontend" --process fixture --module rust --symbols "$work/symbols" \
    -o "$work/rust-fixture" "$root/scripts/tests/llvm_coverage_fixture.rs"
"$inspect" "$work/rust-fixture" "$work/rust-fixture.theseus-coverage.json" | \
    grep -E '^instrumented: [1-9][0-9]* LLVM coverage guards$' >/dev/null
rust_build=$(sed -n 's/.*"build_sha256": "\([0-9a-f]*\)".*/\1/p' \
    "$work/rust-fixture.theseus-coverage.json")
rust_records=$("$work/rust-fixture" 0 2>&1 || true; "$work/rust-fixture" 7 2>&1 || true)
printf '%s\n' "$rust_records" | grep -E \
    "^THES:COV:v2:fixture:rust:${rust_build}:[1-9][0-9]*:0x[0-9a-f]+$" >/dev/null

"$clang_frontend" --process fixture --module plugin --shared --symbols "$work/symbols" \
    -o "$work/plugin.so" "$root/scripts/tests/llvm_coverage_plugin.c"
clang -O2 -g -Wl,--build-id "$root/scripts/tests/llvm_coverage_loader.c" -ldl -o "$work/loader"
plugin_build=$(sed -n 's/.*"build_sha256": "\([0-9a-f]*\)".*/\1/p' \
    "$work/plugin.so.theseus-coverage.json")
plugin_records=$("$work/loader" "$work/plugin.so" 7 2>&1)
printf '%s\n' "$plugin_records" | grep -E \
    "^THES:COV:v2:fixture:plugin:${plugin_build}:[1-9][0-9]*:0x[0-9a-f]+$" >/dev/null

if "$clang_frontend" --process 'invalid:name' --module c -o "$work/invalid" \
    "$root/scripts/tests/llvm_coverage_fixture.c" >/dev/null 2>&1; then
    echo 'LLVM coverage frontend accepted a record delimiter' >&2
    exit 1
fi
