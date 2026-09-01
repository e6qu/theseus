# Theseus CLI

`theseus` is the local entry point for a self-contained Theseus test
directory. It validates the test contract, runs one Firecracker timeline, and
preserves a self-contained replay bundle.

## Commands

```sh
theseus validate [theseus.toml]
theseus test --dry-run [theseus.toml]
theseus test [--output replay-dir] [theseus.toml]
theseus replay replay-dir
theseus explore [--output exploration-dir] [theseus.toml]
theseus explore --replay exploration-dir [--output exploration-dir]
theseus explore --replay exploration-dir --seed-path seed,... [--output exploration-dir]
theseus explore --minimize exploration-dir --seed-path seed,... [--output exploration-dir]
theseus explore --snapshot exploration-dir --seed-path seed,... [--output snapshot-dir]
theseus report [--output report-dir] result-dir
theseus compose validate [compose.yaml]
theseus compose plan [compose.yaml]
theseus compose test [--output replay-dir] [compose.yaml]
theseus compose replay replay-dir [--output replay-dir]
```

`validate` checks the manifest and artifacts. `test --dry-run` prints the
normalized plan, including SHA-256 digests. It does not need KVM and does not
start a VM.

`test` runs one Linux+KVM Firecracker timeline. It copies the runtime binary,
kernel, and initramfs into an immutable replay directory before booting, then
writes the resolved source plan, bundle-local replay plan, serial log,
Firecracker log, and result there. The default output is
`theseus-replay/` beside the manifest; pass `--output` to choose another empty
directory. `replay` uses only the copied artifacts and leaves its source bundle
unchanged.

The CLI is released for Linux amd64/arm64 and macOS arm64. macOS supports
validation and planning only: a Firecracker timeline needs Linux and KVM.

## Explore an SDK guest

`theseus explore` branches a single guest through the in-process control
channel. It needs the published Linux runtime bundle, KVM, and a guest that
uses `theseus-sdk` to send `SETUP_COMPLETE` and a done marker. It leaves an
output directory even if exploration fails, so the locked plan and
`result.json` are available for inspection.

`report` turns an existing replay, Compose topology replay, or exploration
directory into one offline `index.html`. It embeds no external assets and reads
only files within the selected result directory. Open it directly in a browser:

```sh
theseus report theseus-replay
open theseus-replay/theseus-report/index.html
```

The report shows checks and serial logs for one timeline, service checks and
applied faults for a topology, and the search tree plus dirty-page coverage
proxy for an exploration. Every report includes a copy-paste command that
replays only the locked artifacts in that result directory.

```toml
[explore]
max_nodes = 7
branches_per_node = 2
max_depth = 2
rendezvous = true
branch_event_suffix = true
novelty = "markers" # or "coverage"
events = ["90"]
```

`max_nodes` is a hard cap, including the root. `markers` ranks children by
new SDK marker bytes; `coverage` ranks by a deterministic dirty-page coverage
proxy. Every result node records its seed path, marker stream,
entropy probe, and dirty-page count. Use a seed path as the replay recipe;
P6.8 will add the static timeline viewer.

## Test directory

Keep the manifest, extracted published Theseus runtime bundle, kernel, and
initramfs in one directory. Paths in the manifest are relative to that
directory; paths that escape it are rejected.

```text
my-test/
├── theseus.toml
├── runtime/firecracker
└── guest/
    ├── vmlinux
    └── initramfs.cpio.gz
```

Use the Firecracker binary from an extracted, SHA-addressed Theseus release
bundle. Do not point the manifest at a source checkout.

```toml
version = 1

[runtime]
firecracker = "runtime/firecracker"

[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs.cpio.gz"

[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128
timeout_secs = 30

[run.virtual_time]
tick_ns = 1000000
exits_per_tick = 1024

[[events]]
when = "ready"
data = "0100ff"

[network]
loopback = true
drop_ppm = 0
partitioned = false

[[storage]]
id = "data"
size_mib = 64
error_ppm = 0
latency_rounds = 2
torn_write_bytes = 512
corrupt_read_xor = 1

[[checks]]
name = "finished"
kind = "serial_contains"
value = "finished work"

[[checks]]
name = "no panic"
kind = "serial_not_contains"
value = "panic"

[[checks]]
name = "round completed"
kind = "marker_seen"
value = "ff"
```

`events.data` is an even-length hexadecimal byte string. Version 1 has one
delivery point: `ready`, after the guest announces that it can receive input.
The replay bundle preserves the resulting plan verbatim. The runner delivers
serial bytes only after the `THES:M:42` ready marker.
Network drop and partition settings are recorded but intentionally rejected by
this single-VM runner. Simulated storage is also recorded in the plan but runs
only through the Linux Compose executor below. Each entry creates a memory-only
virtio disk: `error_ppm` injects deterministic I/O errors, `latency_rounds`
delays requests by topology pumps, `torn_write_bytes` preserves only a write
prefix while reporting success, and `corrupt_read_xor` changes returned bytes.
Use the Compose planner below to run either topology feature.

## Compose topology planning

Use a small, strict Compose subset to describe a set of Theseus test
directories. Each service names its own `theseus.toml`; the plan locks the
runtime, kernel, and initramfs digest for every service. It accepts only named
networks and `x-theseus.manifest`. Docker images, ports, volumes, host
networks, `depends_on`, and other host-oriented Compose features are rejected.

```yaml
name: example

services:
  api:
    x-theseus:
      manifest: api/theseus.toml
      faults:
        - at_round: 12
          kind: pause
          duration_rounds: 3
        - at_round: 24
          kind: restart
        - at_round: 36
          kind: clock_jump
          nanoseconds: 1000000000
    networks: [backplane]
  worker:
    x-theseus:
      manifest: worker/theseus.toml
    networks: [backplane]

networks:
  backplane: {}
```

Run these commands from the directory containing `compose.yaml`:

```sh
theseus compose validate
theseus compose plan
theseus compose test
```

`compose plan` prints the immutable service artifact plans and sorted network
membership. It also records each memory-only simulated storage device and its
derived seed. `compose test` uses the `theseus-topology` executor included in a
published Linux runtime bundle. It copies and re-checks each service’s
Firecracker, kernel, and initramfs before booting; then it connects service
NICs through an in-process deterministic switch and pumps them in sorted
service-name order. `at_round` is a global scheduler round, not elapsed host
time. Faults are scoped to the service that declares them and must be strictly
ordered. `pause` resumes after `duration_rounds`; `restart` cold-boots from
locked artifacts; and `clock_jump` advances the guest's enabled virtual clock
by `nanoseconds`. The replay directory contains `replay-plan.json` and, for
each service, locked artifacts, one serial log per boot, applied faults, and
`result.json`.

`compose test` needs Linux and KVM. macOS keeps supporting `compose validate`
and `compose plan`; it reports a direct missing-runner error for execution.

## Checks

One-timeline results have two built-in checks: `guest_exit` requires exit
status zero, and `completion` requires exit before `timeout_secs`. Add named
checks in the manifest for the behavior that matters to your system:

- `serial_contains` — a UTF-8 string must appear in `serial.log`.
- `serial_not_contains` — a UTF-8 string must not appear in `serial.log`.
- `marker_seen` — a Theseus marker such as `THES:M:ff` must appear. Give the
  byte(s) after `THES:M:` as `value`.
- `marker_not_seen` — a Theseus marker such as `THES:M:ee` must not appear.
  Give the byte(s) after `THES:M:` as `value`.

`result.json` records every check, its pass/fail status, and a concise detail.
Names must be unique; `guest_exit` and `completion` are reserved.

During `theseus explore`, every check applies to every captured timeline.
`marker_seen` and `marker_not_seen` use a single two-digit hexadecimal byte;
`serial_contains` and `serial_not_contains` use UTF-8 text. The bundle stores
each timeline's console at `serial/<seed>.log` and the static report shows it.
Use `theseus explore --replay exploration-dir --seed-path seed,...` to replay
one recorded root-to-node path without creating its siblings. Theseus verifies
its recorded entropy, marker, and dirty-page fingerprints before accepting it.
Use `theseus explore --minimize exploration-dir --seed-path seed,...` to reduce
a property-failing path to a deterministic 1-minimal event sequence.
Use `theseus explore --snapshot exploration-dir --seed-path seed,...` to export
the selected paused timeline as `snapshot.state` and `snapshot.memory` alongside
its locked artifacts and `snapshot.json` metadata. It exports a snapshot; loading
or modifying snapshots is outside the CLI's scope.
