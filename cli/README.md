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
theseus replay --output diagnostics-dir replay-dir
theseus explore [--output exploration-dir] [theseus.toml]
theseus explore --replay exploration-dir [--seed-path seed,...] [--output exploration-dir]
theseus explore --minimize exploration-dir --seed-path seed,... [--output exploration-dir]
theseus explore --snapshot exploration-dir --seed-path seed,... [--output snapshot-dir]
theseus report [--output report-dir] result-dir
theseus report --format markdown|json|junit|github [--output file] result-dir
theseus compare left-campaign-dir right-campaign-dir
theseus compare --format json|markdown|github left-campaign-dir right-campaign-dir
theseus compare --query /json/pointer left-campaign-dir right-campaign-dir
theseus compare --at-moment <vtime_ns>@<input_sha256> left-campaign-dir right-campaign-dir
theseus query campaign-dir --moment <vtime_ns>@<input_sha256> [--next | --previous] [--format json]
theseus query campaign-dir --list [--service NAME] [--format json]
theseus evaluate [--format json|markdown] [theseus-evaluation.toml]
theseus evaluate lock [theseus-evaluation.toml]
theseus evaluate capture campaign-dir --output evaluation-dir --name name
theseus evidence verify native-evidence.json
theseus coverage cargo --process NAME --module NAME --bin NAME --symbols DIR --output FILE
    [--manifest-path Cargo.toml] [--package NAME] [--release] [--locked] [--offline]
    [--no-default-features] [--features FEATURES] [--target-dir DIR]
theseus coverage go --process NAME --module NAME --package PACKAGE --symbols DIR --output FILE
    [--goarch amd64|arm64] [--tags TAGS] [--mod readonly|vendor] [--offline] [--target-dir DIR]
theseus compose validate [compose.yaml]
theseus compose plan [compose.yaml]
theseus compose test [--output replay-dir] [compose.yaml]
theseus compose explore [--max-runs N] [--guidance MODE] [--output campaign-dir] [compose.yaml]
theseus compose explore --expect-counterexample property [--max-runs N] [--guidance MODE] [--output campaign-dir] [compose.yaml]
theseus compose explore --minimize campaign-dir [--output minimized-dir]
theseus compose explore --minimize campaign-dir --expect-counterexample property [--output minimized-dir]
theseus compose replay replay-dir [--output replay-dir]
theseus compose verify checkpoint-bundle-dir
```

`validate` checks the manifest and artifacts. `test --dry-run` prints the
normalized plan, including SHA-256 digests. It does not need KVM and does not
start a VM.

`test` runs one Linux+KVM Firecracker timeline. It copies the runtime binary,
kernel, and selected guest input into an immutable replay directory before booting, then
writes the resolved source plan, bundle-local replay plan, serial log,
Firecracker log, and result there. The default output is
`theseus-replay/` beside the manifest; pass `--output` to choose another empty
directory. `replay` uses only the copied artifacts and leaves its source bundle
unchanged.

New single-timeline bundles use `theseus-replay-plan-v2` and require
`execution.json`: the complete ordered machine trace, local and VM-wide
digests, terminal boundary, and any active replay error. Manifest UART events
are admitted as recorded host decisions through the API. Replay installs that
trace before boot and rejects changed device effects or a missing suffix; a
matching printed value alone is insufficient. Older version-1 plans retain
legacy input replay without claiming machine-stream enforcement.
Virtual-time runs also write `timer-observations.json` beside
`execution.json`: each in-kernel LAPIC-timer or arch-timer delivery observed
at a handled-exit boundary. KVM injects those interrupts inside the irqchip
without a KVM exit, so they never enter the machine stream; they are evidence
only, replay never gates them, and a replayed bundle's `execution.json` stays
byte-identical without them.

Set `hold_kernel_timers = true` under `[run.virtual_time]` (amd64 only) to go
one step further: the vCPU clears an asserted LAPIC-timer delivery, queues the
held vector, and injects it through KVM_INTERRUPT at its recorded stream turn
(`vcpu:<id>:interrupt:lapic-timer:<vector>`), which replay gates exactly like
any other recorded vCPU turn. Arming still depends on guest counter reads over
host drift, so episodes that move across boundaries between the original run
and its replay diverge instead of silently passing.
Version-2 plans boot with `quiet loglevel=0`, as topology runs do: kernel
diagnostics can contain uncontrolled host-clock values. All device decisions
made under that boot policy are still checked.
`run.entropy_device` defaults to `true`. Set it to `false` for a guest that
does not need the seeded virtio RNG; the choice is locked in its replay plan.
Boot-time Linux allocation and hardware entropy can still change RNG queue
addresses. Active replay rejects those changes; a seed alone does not control
the kernel's boot behavior.
Checkpoint exploration requires the seeded RNG for its branch probes and
rejects `entropy_device = false` before boot.

Use `replay --output` to retain diagnostics in a new named directory. It writes
serial and Firecracker logs, fresh execution evidence, and `result.json` even
when replay fails. It does not copy another executable bundle. The source
bundle remains unchanged. Timeout cuts are paused and flushed before process
termination; their host-timed boundary is diagnostic, not actively replayable.

The CLI is released for Linux amd64/arm64 and macOS arm64. macOS supports
validation and planning only: a Firecracker timeline needs Linux and KVM.

`evidence verify` is an offline check for a native-certification release set.
It requires the index plus the certificate, portable counterexample, and
runtime-validation archive for every architecture named by that index. It
verifies their hashes and complete file inventories without KVM. It then checks
the fixed-plan certificate, the partition/recovery/lost-update replay, and the
container, coverage, schedule-search, pthread-synchronization, and ordered
KVM-exit replay runs. An index may certify amd64, arm64, or both; the command
reports the exact scope. Version 2 adds per-vCPU execution ledgers, version 3
adds a machine-wide ledger, and version 4 requires the exact active-replay
trace for every retained run and service. That trace may contain `vcpu:` exits
and `host:` UART, control-channel, or clock-jump effects. The verifier keeps
reading the earlier formats without treating their absent fields as evidence.
Runtime-validation version 5 additionally requires standalone container-run
and replay evidence, recomputes their machine and local digests from the full
traces, and verifies the passing active-replay check through guest exit.

## Explore an SDK guest

`theseus explore` branches a single guest through the in-process control
channel. It needs the published Linux runtime bundle, KVM, and a guest that
uses `theseus-sdk` to send `SETUP_COMPLETE` and a done marker. It leaves an
output directory even if exploration fails, so the locked plan and
`result.json` are available for inspection. It also locks the companion
`theseus-explorer` binary into `artifacts/`; replay uses that digest-checked
copy instead of a newer runner installed beside `theseus`.

Use `[[events]]` for deterministic UART input. Theseus injects each event
directly into every timeline after `SETUP_COMPLETE`; do not use host stdin.
Use `[explore].events` only for SDK control-channel bytes.

`report` turns an existing replay, Compose topology replay, or exploration
directory into one offline `index.html`. It embeds no external assets and reads
only files within the selected result directory. Open it directly in a browser:

```sh
theseus report theseus-replay
open theseus-replay/theseus-report/index.html
```

### Investigate two campaign results

Compare two locked Compose campaign directories without starting a VM or
opening a snapshot. Theseus stops at the first recorded difference: selected
operation or fault, applied fault action, operation-boundary topology state,
serial evidence, coverage, thread-scheduling choice, or property outcome.

```sh
theseus compare campaign-before campaign-after
theseus compare --format markdown campaign-before campaign-after > investigation.md
theseus compare --query /runs/0/timeline/1/serial_sha256 \
  campaign-before campaign-after
```

`--query` accepts an RFC 6901 JSON Pointer and returns that retained field from
both `campaign-result.json` files. `--at-moment` dumps both sides' complete
boundary record at one moment address — everything that differs at that exact
point, not just the first divergence the comparison stops at:
`theseus compare --at-moment 7000@0f1c2b3d left right`. Use it for a specific operation boundary,
fault action, topology hash, serial digest, coverage location, or property
verdict. The comparison and its Markdown form contain no VM memory or external
service dependency, so attach them with the two locked result directories.

### Summarize a public evaluation

A versioned evaluation names campaign bundles within its own directory and
states the expected campaign and property outcomes. Summarize it without KVM:

```sh
theseus evaluate evaluations/replicated-counter/theseus-evaluation.toml
theseus evaluate --format markdown evaluations/replicated-counter/theseus-evaluation.toml
```

The result counts replay verification, corpus and coverage evidence,
thread-scheduling decisions, checkpoint work, reduction work, and retained
operation boundaries. A suite
may include a conventional-chaos baseline and manually observed investigation
seconds, but Theseus labels those informational: neither affects a replay
verdict or proves a comparison with another product.

A version-2 evaluation names a `lockfile`. `evaluate lock` resolves every
declared workload bundle and writes that lockfile with the complete file
inventory of each bundle, so later evaluation can reject a modified artifact:

```sh
theseus evaluate lock evaluations/replicated-counter/theseus-evaluation.toml
```

`evaluate capture` is the publication boundary for a campaign produced on a
KVM runner. It requires a complete, replay-verified Compose campaign bundle,
copies it into a new self-contained evaluation directory, writes a version-2
evaluation naming the observed campaign and property outcomes, and locks every
copied artifact:

```sh
theseus evaluate capture theseus-compose-campaign \
  --output evaluations/my-system --name my-system
theseus evaluate evaluations/my-system/theseus-evaluation.toml
```

The report shows checks and serial logs for one timeline, service checks and
applied faults for a topology, and the search tree plus dirty-page coverage
proxy for an exploration. Every report includes a copy-paste command that
replays only the locked artifacts in that result directory.

Campaign reports also explain paused-vCPU instruction samples from the locked
service kernel ELF as `address → function + offset`. The address is still the
replay identity and is used when symbols are absent, so a stripped kernel never
changes campaign coverage or prevents replay.

A Compose campaign can select `coverage: application_blocks` when its C
commands were built with `theseus-coverage-cc` or its Go commands were built
with `theseus coverage go`. Reports list service/process/module/build/block
identities separately from sampled guest PCs, and replay checks the exact
retained block set and novelty.

Use `coverage: application_edges` for binaries built by the packaged
`theseus-coverage-clang`, `theseus-coverage-rustc`, or `theseus coverage cargo`
frontend. Edge records add
a build-local guard number to the module-relative address. C, C++, single-file
Rust programs, PIE executables, and dynamically loaded native modules use the
same replay path. Each module retains at most 65,535 edge identities.
Preserve symbol files with `--symbols`; `theseus-coverage-inspect` validates the
build and resolves an observed offset to a source line. Declare each manifest
and its symbol directory on the service so planning locks the exact files and
campaign reports resolve functions and source lines automatically:

```yaml
services:
  api:
    x-theseus:
      manifest: api/theseus.toml
      coverage:
        - manifest: api/work/api.theseus-coverage.json
          symbols: api/work/symbols
```

Both paths are relative to the Compose file. The manifest names the exact
build-scoped file inside `symbols`.

On Linux, build a Cargo binary and its static Rust target dependency graph
without replacing each crate's compiler command:

```sh
theseus coverage cargo \
  --manifest-path Cargo.toml --package api --bin api \
  --process api --module command --symbols work/symbols --output work/api \
  --release --locked
```

The command resolves the selected graph, hashes each package tree and the
workspace build configuration, isolates the build under a build-scoped target
directory, preserves an unstripped symbol file, and writes
`work/api.theseus-coverage.json`. Pass `--features` and
`--no-default-features` to resolve a specific Cargo feature set, and
`--target-dir` to place that isolated build under a chosen directory instead
of the workspace default. Host build scripts and proc macros are build
inputs but are not instrumented; Rust dynamic-library targets still need
independent module handling. See Tutorials 37 and 38.

Build one Go command and every imported source package in its main module from
an isolated source copy:

```sh
theseus coverage go \
  --package ./cmd/api --process api --module command \
  --symbols work/symbols --output work/api --offline
```

The command targets a fixed-address Linux ELF for amd64 or arm64, disables
CGO, preserves an unstripped symbol file, and writes
`work/api.theseus-coverage.json`. Each basic block emits its absolute program
counter once. `--target-dir` places the isolated build under a chosen
directory; `--goarch`, `--tags`, and `--mod` select the target architecture,
build tags, and module resolution mode. External module packages are build
inputs but are not instrumented. Declare the manifest and symbols in the same
coverage catalog; the runner validates the Go callback and debug data before
boot and reports source locations. See Tutorial 39.

Commands built with the packaged `theseus-schedule-cc` frontend accept a
Compose shell operation's explicit `thread_schedule: [0, 1, 2]`. Planning
validates the bound and locks it as `THESEUS_THREAD_SCHEDULE` for the command.
The same field accepts a bounded search contract such as
`{threads: [0, 1, 2], period: 5, max_switches: 3}`. Planning expands at most
256 periodic patterns into named, immutable input cases; normal campaign
selection, minimization, and replay then operate on those cases.
Use `{runnable_prefixes: {max_choices: 32, max_variants: 256}}` to start with
an empty choice prefix and fork only alternatives found in runtime runnable
masks. Both bounds are locked in the plan; each executed prefix is retained in
the campaign result and reused by minimization and replay.
Campaign results and reports retain each build-scoped scheduling point,
runnable mask, current thread, and selected thread. Replay rejects a changed
sequence. This bounded GCC C path is not a general Linux scheduler; see
Tutorials 32–34 for its limits.

A shell operation may also declare bounded, named `choices`:

```yaml
shell:
  command: [/usr/local/bin/chooser]
  choices: {mode: 2, retry: 3}
```

Planning locks every assignment and supplies it as `THESEUS_CHOICES`. The
command emits `THES:CHOICE:<name>:<upper-bound>:<value>` immediately before it
uses a value; `theseus-sdk` provides `TtyChannel::choice`, and plain programs
can use the same line protocol. Unified guidance combines choice and schedule
decisions with coverage, property, fault, and topology-state feedback. Results
retain a human-readable decision trace, and replay rejects divergence.

### Compose a test from lifecycle commands

Package executable commands in an image using the standard test-template
layout:

```text
/opt/antithesis/test/v1/main/
  first_prepare
  parallel_driver_write
  serial_driver_inspect
  eventually_recovered
```

```yaml
x-theseus:
  campaign:
    driver: client
    max_parallel_commands: 3
    max_operations_per_run: 10
    faults: []
```

With no explicit `operations` or `test_template`, Theseus discovers every
template across the locked service images and selects exactly one per generated
timeline. Set `test_template: main` to restrict the campaign to one template,
or `test_templates: [main, smoke]` to name an allowed set. Theseus requires
recognized commands to be executable and ignores entries whose names start
with `helper_`. It generates
three independently selectable process slots for each parallel or anytime
command in this example. An eventual command kills live test commands and
restores active campaign faults before it runs; a final command waits for all
commands to finish.

Explicit operations remain available when a command needs Compose state,
guards, inputs, or a custom output contract:

```yaml
operations:
  - name: prepare
    command: first
    shell: {command: [/work/prepare]}
  - name: write
    command: parallel_driver
    shell: {command: [/work/write]}
  - name: inspect
    command: serial_driver
    shell: {command: [/work/inspect]}
  - name: verify
    command: finally
    shell: {command: [/work/verify]}
```

The complete vocabulary is `first`, `parallel_driver`, `serial_driver`,
`singleton_driver`, `anytime`, `eventually`, and `finally`. Use shell
`phase: launch` and `phase: completion` on parallel or anytime commands when
the process must span operation boundaries. Theseus excludes histories with
setup out of place, mixed singleton and regular drivers, a serial driver next
to a live parallel process, work after a terminal command, or an unjoined
process. The locked plan, decision trace, replay, minimizer, and report retain
the roles, source command path, chosen concurrency, and eventual termination
services. See Tutorial 40.

### Hand a failure to CI or an issue

The same locked result can be rendered without a browser. `markdown` writes a
short bug report with the replay command, failed properties, minimized input,
and logs; use it directly in a GitHub Actions job summary. `github` writes
`::error`/`::warning` workflow annotations for every failed check plus the
replay command, aimed at `$GITHUB_STEP_SUMMARY`. `junit` emits one test case
per Theseus property for CI systems. `json` emits the versioned
`theseus-report-v1` model for issue bots and other tools. Non-HTML formats go
to stdout unless `--output` names a new file.

```sh
theseus report --format markdown failing-bundle >> "$GITHUB_STEP_SUMMARY"
theseus report --format junit --output theseus-results.xml failing-bundle
theseus report --format json --output theseus-results.json failing-bundle
```

The output is evidence, not the replay bundle. Upload the original locked
directory with it; a recipient runs the printed replay command against that
bundle and needs the matching published Linux runtime to execute it.

```toml
[explore]
max_nodes = 7
branches_per_node = 2
max_depth = 2
run_ms = 100
rendezvous = true
branch_event_suffix = true
novelty = "markers" # or "coverage"
events = ["90"]
```

`max_nodes` is a hard cap, including the root. `run_ms` bounds each captured
timeline's run in milliseconds (default 100). `markers` ranks children by
new SDK marker bytes; `coverage` ranks by a deterministic dirty-page footprint
proxy. Every result node records its seed path, marker stream, entropy probe,
and dirty-page count. Use a seed path as the replay recipe.

## Test directory

Keep the manifest, extracted published Theseus runtime bundle, kernel, and
one guest input in one directory. The guest input is either an initramfs or a
Docker `save` archive. Paths in the manifest are relative to that directory;
paths that escape it are rejected.

```text
my-test/
├── theseus.toml
├── runtime/
│   ├── firecracker
│   └── theseus-image
└── guest/
    ├── vmlinux
    └── service.tar
```

Use the Firecracker binary from an extracted, SHA-addressed Theseus release
bundle. Do not point the manifest at a source checkout.

```toml
version = 1

[runtime]
firecracker = "runtime/firecracker"
image_adapter = "runtime/theseus-image"

[guest]
kernel = "guest/vmlinux"
image = "guest/service.tar"

[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128
timeout_secs = 30

[run.virtual_time]
tick_ns = 1000000
exits_per_tick = 1024
hold_kernel_timers = false

[[events]]
when = "ready"
data = "0100ff"

[network]
loopback = true
drop_ppm = 0
duplicate_ppm = 0
corrupt_ppm = 0
partitioned = false
latency_rounds = 1
jitter_rounds = 1
tx_bytes_per_round = 4096
mtu_bytes = 1500
tx_queue_frames = 64
rx_queue_frames = 64

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

For a prebuilt guest, omit `runtime.image_adapter` and use
`initramfs = "guest/initramfs.cpio.gz"`. `image` and `initramfs` are mutually
exclusive. Image replay locks both the original archive and adapter binary;
it does not rely on a host Docker daemon.

An image-backed service can also declare a `[container_service]` boot
contract — HTTP or gRPC readiness, assertions, operations, shell operations,
and the `campaign` flag — in the service manifest. That contract is injected
into the image's PID 1; see
[the container-images guide](../docs/guides/container-images/README.md).

`events.data` is an even-length hexadecimal byte string. Version 1 has one
delivery point: `ready`, after the guest announces that it can receive input.
The replay bundle preserves the resulting plan verbatim. The runner delivers
serial bytes only after the `THES:M:42` ready marker.
Network settings are recorded but intentionally rejected by this single-VM
runner. The Linux Compose executor supports deterministic drops, duplication,
and one-bit corruption of selected nonempty frames,
partitions, base `latency_rounds`, and seeded per-frame `jitter_rounds`; jitter
can reorder frames without consulting host time. `tx_bytes_per_round` limits
outbound traffic at deterministic topology rounds; zero leaves it unlimited.
`mtu_bytes` drops larger frames before they enter the simulated link; zero leaves it unlimited.
`tx_queue_frames` bounds frames waiting for outbound link budget; zero leaves it unlimited.
`rx_queue_frames` bounds frames waiting for guest delivery; zero leaves it unlimited.
Simulated storage is also recorded in the plan but runs only through that executor. Each
entry creates a memory-only virtio disk: `error_ppm` injects deterministic I/O
errors, `latency_rounds` delays requests by topology pumps,
`torn_write_bytes` preserves only a write prefix while reporting success, and
`corrupt_read_xor` changes returned bytes. Use the Compose planner below to run
either topology feature.

## Compose topology planning

Use a small, strict Compose subset to describe a set of Theseus services.
Every service names its own `theseus.toml` through `x-theseus.manifest`; that
per-service manifest selects either an initramfs or a Docker image archive,
and image-backed services use the same launch fields described in
[the container-images guide](../docs/guides/container-images/README.md). The plan
locks the runtime, kernel, and selected guest-input digest for every service.

Besides `x-theseus` and `networks`, a service may declare `depends_on` (with
`condition: service_healthy`), `environment`, `env_file`, `command`,
`entrypoint`, `working_dir`, `user`, `hostname`, `extra_hosts`, `read_only`,
`tmpfs`, `configs`, `secrets`, bind `volumes`, `healthcheck`, and CPU/memory
limits through `cpus`, `mem_limit`, or `deploy.resources.limits`. Host ports
and host networks are rejected, `image` is not a service field (the archive is
declared in the service manifest), and environment values must be literal:
host-environment inheritance and interpolation have no deterministic meaning.

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
Firecracker, kernel, and selected guest input before booting; then it connects service
NICs through an in-process deterministic switch and pumps them in sorted
service-name order. `at_round` is a global scheduler round, not elapsed host
time. The topology round budget is the largest `[run].max_rounds` across the
service manifests; it defaults to 10000000. Faults are scoped to the service
that declares them and must be strictly
ordered. `pause` resumes after `duration_rounds`; `restart` cold-boots from
locked artifacts; and `clock_jump` moves the guest's enabled virtual clock
by `nanoseconds`; negative values jump backward, saturating at the anchored
tick floor instead of failing the run. The replay directory contains `replay-plan.json` and, for
each service, locked artifacts, one serial log per boot, applied faults, and
`result.json`.

`compose verify` checks a complete ready-checkpoint campaign or standalone
bundle offline: locked inputs, configuration, state/RAM/context hashes,
ancestry, UART logs, and complete execution ledgers. It reports integrity, not
native execution certification. Retain the whole output directory; individual
campaign run directories can share a checkpoint and are not standalone exports.

`compose test` needs Linux and KVM. macOS keeps supporting `compose validate`
and `compose plan`; it reports a direct missing-runner error for execution.

While `compose explore` runs, the executor writes one structured progress
line per completed timeline to stderr: `theseus-progress-v1` with the
completed count, run index, status, operations, faults, failed properties,
checkpoint reuses, and the effective guidance and budget, so CI jobs and
wrappers can follow the search live and attribute it to the comparison arm
that produced it.

Campaign operation-barrier fault entries are explored as optional choices by
default. Add `required: true` when every generated schedule that reaches the
fault's `after` operation must apply it. Required faults count toward
`max_faults_per_run` and counterexample minimization keeps them. This supports
fixed disruption/recovery scenarios without turning all other fault candidates
into mandatory actions.

Operation-barrier faults also cover CPU throttling, directed link clogs, and
guest-clock rates: `cpu_throttle` with `service`, `after`, `duration_rounds`
(1–100000), and `every_n_rounds` (2–64) pumps the service on only 1 of N
topology rounds for the window; `cpu_release` ends it early. `link_clog` with
`network`, distinct `from`/`to`, `after`, and `latency_rounds` (1–4096)
stalls frames on the directed link until `link_unclog`. `clock_rate` with
`service`, `after`, `rate` (2–16), and `duration_rounds` moves that service's
virtual clock rate until `clock_rate_release`; it requires `virtual_time`.
Terminal checks release all three automatically.

Pass `--max-runs N` and `--guidance
coverage|adaptive|posterior|property|unified` to `compose explore` to
override the declared budget and guidance for one exploration without editing
the Compose file: comparing the same file across guidance modes at one fixed
budget is the reproducible search comparison the roadmap requires, and every
retained campaign records which mode and budget produced it.

## Query a moment

Every retained campaign boundary carries a moment address (see the Moment
log in any campaign report). Resolve one address to its boundary, log
excerpts, and neighboring moments:

```sh
theseus query theseus-compose-campaign --moment 7000@0f1c2b3d4e5f60718293a4b5c6d7e8f90112233445566778899aabbccddeeff0
```

Expect the run and boundary identities, the service and operation, the
cumulative virtual time and input digest, the bounded serial excerpts, and
the previous and next moment addresses for temporal navigation. An address
that no boundary carries fails with that address in the error. Walk the
timeline temporally from any address, or list every moment in the bundle:


`--next` and `--previous` resolve the adjacent boundary as a full hit;
`--list` prints every moment address in timeline order with its run,
boundary, and service, and `--service NAME` narrows the index to one
service. `--format json` emits machine-readable output for both modes —
for issue bots and CI triage piping the moment index or a resolved hit
straight into other tooling.

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
When present, it also verifies the timeline's serial-log digest. Without
`--seed-path`, it rebuilds and verifies the entire recorded tree.
Use `theseus explore --minimize exploration-dir --seed-path seed,...` to reduce
a property-failing path to a deterministic 1-minimal event sequence.
Use `theseus explore --snapshot exploration-dir --seed-path seed,...` to export
the selected paused timeline as `snapshot.state` and `snapshot.memory` alongside
its locked artifacts and `snapshot.json` metadata. It exports a snapshot; loading
or modifying snapshots is outside the CLI's scope.
