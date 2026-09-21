# Exploration: the multiverse machinery

How Theseus turns one paused microVM into a tree of timelines. Read
[determinism.md](determinism.md) first; the [control channel](control-channel.md)
doc covers the wire protocol this builds on.

## Branch points

`BranchPoint::capture` freezes a paused microVM entirely in memory:

- the serialized `MicrovmState` (vCPU, devices, clock state), and
- a full guest-RAM dump in a `memfd` (no disk I/O).

Children restore through the unmodified snapshot-restore path with the
memfd mapped `MAP_PRIVATE` — the kernel provides copy-on-write, so sibling
timelines share mapped pages until they write them. A unit test exercises this
mapping behavior. Capture itself still performs one full RAM dump per branch
point, so the complete path is not zero-copy.

Each child is reseeded before resume, so siblings differ *only* by seed
(`splitmix64(base_seed ^ (branch_index << 32))` — deterministic). Fault
schedules are a second divergence axis: the sim-net config is rewritten in the
captured state per child.

## The timeline tree

`TimelineTree` records nodes (seed + captured branch point + fingerprints)
and yields deterministic DFS exploration order; `seed_path(id)` is the
replay recipe for any timeline.

## The explorer

`Explorer::explore` is the live loop:

1. Boot the root timeline (seeded entropy, workload of your choice).
2. Rendezvous: wait for `SETUP_COMPLETE`, push manifest `[[events]]` into the
   emulated UART, then push control-channel events + a terminator, wait for
   the done marker, pause, fingerprint, capture. Every child receives the same
   UART bytes directly from its own VM, never from shared host stdin.
3. Spawn `branches_per_node` children from each branch point — each on its
   own scoped thread (one timeline per thread, results joined in spawn
   order so the tree is deterministic).
4. Recurse depth-first, novelty-ordered: children with markers not yet
   seen in the tree expand first (tie-break: seed).

The parallel fan-out is headless: the vCPU thread handles MMIO
synchronously and pause/probe/capture are `Vmm` methods, so no
`EventManager` has to cross threads (it is not `Send`). Constraint:
parallel timelines must not use host-fd-backed devices; sim backends and
the MMIO door are pump-free by construction.

## Overlapping image commands

A Compose campaign can keep an ordinary image process alive across operation
boundaries. A `shell` operation with `phase: launch` and a service-local
`process` name forks the argv command, records its launch, and checkpoints
without waiting. A later `phase: completion` for that name waits for the exit,
checks its status and bounded output, and records a monotonically increasing
completion-observation position.

The running PID, output pipe, and process memory belong to the guest and are
therefore part of a whole-topology checkpoint. Generated campaign histories
exclude completion-before-launch and a second launch of an already-live name.
The retained timeline assigns every boundary a stable `op-NNN-<name>` ID.
This controls operation-level overlap; it is not general Linux thread
scheduling.

## Test-command lifecycle

A Compose campaign can act as a reusable test template by assigning `command`
to every operation:

```yaml
operations:
  - name: prepare
    command: first
    shell:
      command: [/work/prepare]
  - name: start_writer
    command: parallel_driver
    shell:
      phase: launch
      process: writer
      command: [/work/write]
  - name: join_writer
    command: parallel_driver
    shell:
      phase: completion
      process: writer
  - name: inspect
    command: serial_driver
    shell:
      command: [/work/inspect]
  - name: verify
    command: finally
    shell:
      command: [/work/verify]
```

The accepted roles are `first`, `parallel_driver`, `serial_driver`,
`singleton_driver`, `anytime`, `eventually`, and `finally`. Every generated
timeline obeys these rules:

- Exactly one declared `first` command starts the timeline. With several
  alternatives, the explorer chooses one.
- Parallel and serial drivers never share a timeline with a singleton driver.
- A template that declares drivers does not retain timelines containing only
  anytime checks. A singleton timeline runs exactly one singleton driver.
- A serial or singleton driver cannot start while a parallel process launched
  by the template remains live. An anytime command may run in that window.
- Eventually and finally commands are terminal. A final command starts only
  after every named process has been joined. An eventual command kills live
  test commands across their image services and restores active campaign
  faults before it starts.
- A retained timeline never ends with an unjoined named process.
- Operation-barrier faults cannot target first, eventually, or finally
  commands.

Omit `operations` and `test_template` to discover every template directly from
executable files in `/opt/antithesis/test/v1/<template>/` inside image
services. Each generated timeline selects exactly one template. Set
`test_template` to select one directory, or `test_templates` to restrict
discovery to a named set. Theseus recognizes the seven standard filename
prefixes, ignores `helper_` entries, rejects recognized files without an
executable bit, and merges each template across services.
`max_parallel_commands`
bounds the process slots generated for every parallel and anytime command; the
explorer chooses which slots run.

The ordinary operation rules still apply: inputs, guards, state transitions,
structured choices, faults, properties, guidance, minimization, and replay all
compose with the lifecycle. Explicit operations can still use the named-process
launch/completion protocol. Tutorial 40 uses image discovery without a host
orchestration script or a duplicated command list.

## Generate ordinary distributed-system faults

Add one field instead of enumerating every service, directed link, and command
boundary:

```yaml
x-theseus:
  campaign:
    driver: api
    fault_profile: standard
    max_faults_per_run: 2
```

The `standard` profile inspects the locked Compose topology. At eligible driver
and anytime boundaries it generates service stop, kill, and restart choices
for image services, plus a bounded CPU throttle (16 rounds at 1 of every 4)
per image service. For every ordered pair on a shared network, it also
generates a directed partition, a directed degradation with 10% loss, 1%
duplication, 0.1% corruption, two rounds of latency and jitter, 4096 bytes per
round, a 1200-byte MTU, eight-frame transmit and receive queues, and a
directed link clog that stalls frames for 64 rounds. Each image-backed
service with virtual time also gains a bounded clock-rate candidate (32
rounds at 4x).

The expansion is deterministic and capped at 512 candidates. The complete
catalog is written to the plan; `max_faults_per_run` still bounds each
timeline. Theseus does not generate faults after setup, completion, first,
eventually, finally, assertion, or recovery commands. It restores stopped or
killed services and directed network conditions before terminal checks.

Use explicit `faults` alongside the profile when a test needs a different
target or condition. The additional operation-boundary kinds are
`service_stop`, `service_start`, `service_kill`, `service_restart`,
`link_fault`, `link_recover`, `cpu_throttle`, `cpu_release`, `link_clog`, and
`link_unclog`. A link fault accepts the same packet-condition
fields as `network_fault`, plus distinct `from` and `to` services on its
network.

`cpu_throttle` modulates a service's CPU at the scheduler boundary: for
`duration_rounds` topology rounds after the named operation, the service is
pumped only on 1 of every `every_n_rounds` (2–64) rounds, so its guest runs at
a reduced, still-deterministic share. `cpu_release` ends the throttle early;
terminal checks release any active throttle automatically. `clock_rate`
requires `virtual_time` on the target service and moves its guest clock rate
to `rate`× (2–16) for `duration_rounds` rounds; `clock_rate_release` restores
1× early, and terminal checks do the same. The rate is a recorded host effect
like a clock jump, so replay gates it exactly. `link_clog` jams a
directed link with a bounded stall: frames still enter the link, but each
waits `latency_rounds` (1–4096) before delivery, so clogged frames queue up
and arrive only after `link_unclog` (or terminal recovery) restores the link.

## Require a fault and recovery path

Operation-barrier faults are optional search choices unless they set
`required: true`. A required fault is present in every generated schedule that
reaches its `after` operation, counts toward `max_faults_per_run`, and is
preserved by counterexample minimization. Use required actions when the
workload must prove that a specific disruption and recovery happened before
testing the property:

```yaml
max_faults_per_run: 2
faults:
  - kind: partition
    required: true
    network: backplane
    after: start
  - kind: heal
    required: true
    network: backplane
    after: verify_isolation
```

Keep the property independent of the fault when that distinction matters. For
example, Tutorial 30 heals and probes the network before it overlaps two
writes, so the retained lost update is still a concurrency failure.

When a campaign deliberately contains a property to falsify, make that outcome
part of the command contract:

```sh
theseus compose explore --expect-counterexample lost_update \
  --output campaign compose.yaml
theseus compose explore --minimize campaign \
  --expect-counterexample lost_update --output minimized
```

These commands succeed only when the runner finishes and retains the named
failed property or its completed minimization. A runner crash, a different
failed property, or an unexpectedly passing campaign still returns nonzero.

## Properties

Add `marker_seen`, `marker_not_seen`, `serial_contains`, or
`serial_not_contains` checks to an exploration manifest. Marker values are one
hexadecimal byte; serial values are UTF-8 text. Each check applies to every
captured timeline, not merely the root. A failed result names the first seed
paths that violated it. The bundle records each timeline's serial console as
`serial/<seed>.log`, and the static report shows those logs.

## Operation state and phases

A campaign can constrain when an operation may run and what it records, so a
generated timeline reads as an intentional scenario:

```yaml
x-theseus:
  campaign:
    driver: counter
    state: {phase: new, worker: idle}
    operations:
      - name: setup
        max_uses: 1
        requires_state: {phase: new}
        sets_state: {phase: partitioned}
        shell:
          phase: setup
          command: [/bin/sh, -c, "rm -f /state/*"]
      - name: recover
        requires_state: {phase: inspected}
        sets_state: {phase: recovered}
        shell:
          phase: recovery
          command: [/bin/sh, -c, "mkfifo /state/ready"]
```

`requires_state` and `sets_state` are ordered key/value maps over the
campaign state; planning only generates an operation when its requirements
match the current state, then applies its updates. `max_uses` bounds how many
times one operation may appear in a timeline. The shell `phase` vocabulary is
`run` (default), `setup`, `launch`, `completion`, `assertion`, and
`recovery`; `launch`/`completion` form the named-process overlap protocol
described above, and the remaining phases exist to make a retained scenario
readable. Tutorial 30 uses this contract for its partition, probe, and
recovery steps.

## Campaign properties

A Compose campaign declares named properties under `x-theseus.campaign`.
Every generated timeline is evaluated against them, and `kind` selects the
quantifier over the corpus:

- `always` — every generated timeline must report the property.
- `always_or_unreachable` — every timeline must report it, or none may reach
  it at all; a corpus where only some timelines report it fails. Use it when
  the instrumented path may legitimately never execute.
- `sometimes` — at least one generated timeline must report it.
- `reachable` — the campaign must reach a timeline that reports it.
- `unreachable` — no generated timeline may report it.

A property observes the ordered serial transcripts of its service (or the
whole topology) with:

- `contains`, `contains_all`, `contains_any`, `contains_none` — literal
  substring requirements over the transcript lines.
- `predicate` — a structured JSON predicate evaluated against one
  JSON-lines event (`output_json: true` on a shell operation emits one).
  It accepts RFC 9535 `query`, exact `fields` matches, `where` comparators,
  array predicates, and nested `all`/`any`/`none` groups.
- `requires_serial_all`, `requires_serial_any`, `excludes_serial_any` —
  guards that must (or must not) appear before the observing operation, each
  matching a `contains` substring, a `matches` regex, or a JSON predicate on
  a named service.
- `requires_serial_correlations` — JSON Pointer values that must agree
  between two service transcripts, such as a request ID echoed by a replica.
- `requires_serial_joins` / `excludes_serial_joins` — values from one
  endpoint's transcript that must (or must not) occur in every or any other
  endpoint, with optional `quantifier` and `occurs` bounds.
- `requires_serial_evidence` / `excludes_serial_evidence` — composable
  `all`/`any`/`none` groups over the guards, correlations, joins, and
  relations above.

```yaml
properties:
  - name: network_recovery_is_reachable
    kind: reachable
    service: writer-a
    predicate:
      json:
        fields: {/event: shell_operation, /name: verify_recovery, /output/network: recovery_probe_sent}
  - name: distributed_lost_update_is_unreachable
    kind: unreachable
    service: counter
    predicate:
      json:
        fields: {/event: shell_operation, /name: inspect, /output/value: 1}
```

Every operation boundary in a retained campaign carries a moment address —
`<virtual-time-ns>@<input-sha256>` for the service that received the
operation — so reports, queries, and cross-run comparisons can reference one
stable point in one run's event log. The campaign report renders a moment
log: an index from every moment address to its bounded log excerpt, and
`theseus query <campaign-dir> --moment <address>` resolves an address to its
boundary, log excerpts, and neighboring moments for temporal navigation.

Planning locks the complete property set, and campaign results retain each
verdict. A failed verdict names the first violating timeline and a bounded
serial excerpt around the primary needle, so a report leads directly from
the failed property to the relevant log text. When two retained campaigns
diverge, `theseus compare` reports both sides' moment addresses at the
diverging boundary, so the same address retrieves either run's log point. `--expect-counterexample` succeeds only when the named property is
retained as failed, and its minimization preserves that outcome. Tutorial 30
is the worked example. `compare` reports differing property
verdicts between two campaigns; it remains an observation, not a causal
result.

## Structured choices and unified guidance

Declare bounded choices on a Compose shell operation:

```yaml
guidance: unified
operations:
  - name: calculate
    shell:
      command: [/usr/local/bin/chooser]
      choices: {mode: 2, retry: 3}
```

Planning expands the finite product into locked input cases and sets
`THESEUS_CHOICES` to comma-separated `name=value` assignments. Consume a value
at the decision point and emit `THES:CHOICE:<name>:<upper-bound>:<value>`
immediately before using it. The Linux SDK's `TtyChannel::choice` implements
the same protocol, but any language can emit the line directly.

`theseus compose explore --max-runs 64 --guidance coverage compose.yaml`
overrides the declared budget and guidance for one exploration without
editing the Compose file. Comparing one file across guidance modes at one
fixed budget — retaining every campaign — is the reproducible search
comparison the roadmap requires.

The `unified` policy is the default for newly planned Compose campaigns. It
ranks a common decision-prefix tree using application and VM coverage,
property witnesses, topology novelty, structured-choice novelty, runnable-set
decisions, failures, and an exploration bonus. Each run retains a canonical
decision trace covering operation input, observed choices, thread selections,
and applied actions. Replay checks that trace and the typed evidence. This is a
bounded campaign decision plane; it does not yet control arbitrary Linux
process, futex, syscall, timer, or interrupt scheduling.

## Reproduce one timeline

Every timeline in a static exploration report includes a copyable command for
its seed path. It replays the root and only the selected child at each branch;
it does not rerun siblings or the whole search tree:

```sh
theseus explore --replay exploration-dir --seed-path 42,123,456 \
  --output timeline-replay
```

The path starts with the root seed. New bundles also carry the exact published
`theseus-explorer` binary used to create them; replay verifies its digest and
uses that bundle-local copy. Theseus rejects a path that does not match the
locked branch contract. It also compares the selected timeline's entropy
probe, markers, and dirty-page count with the recorded result. A mismatch makes
the replay fail and is shown in its static report. When the bundle contains a
serial log, Theseus also verifies its SHA-256 digest.

Replay without `--seed-path` rebuilds the full recorded tree and verifies every
seed path and fingerprint, including serial-log digests when available. It
fails if the search produces a different tree or any captured timeline differs.

## Minimize a failing path

After a marker property fails, minimize its base event sequence:

```sh
theseus explore --minimize exploration-dir --seed-path 42,123,456 \
  --output minimized
```

Theseus removes events greedily while preserving the exact set of failed named
checks. The result is deterministic and **1-minimal**: no remaining individual
event can be removed. It does not claim a globally smallest sequence. The
minimized bundle contains its locked plan and records both event sequences.

## Export a paused timeline

Export the captured state for one seed path when you need to inspect it with
Firecracker snapshot tooling:

```sh
theseus explore --snapshot exploration-dir --seed-path 42,123,456 \
  --output paused-timeline
```

The output is self-contained. `snapshot.state` and `snapshot.memory` use the
Firecracker snapshot-file layout; `snapshot.json` records their names, the seed
path, and the node fingerprints. It also retains the locked Theseus artifacts and
plan. This command exports the snapshot only; it does not load, mutate, or debug it.

## Fingerprints per node

Every captured node records three fingerprints (see
[determinism.md](determinism.md#replay-fingerprints)): entropy probe,
markers, dirty-page count. Running the same exploration twice must
reproduce all three at every node — `test_explore_is_deterministic`
asserts it.

## Coverage

`coverage.rs` collects guest PCs via `KVM_GUESTDBG_SINGLESTEP`. This is an
instruction-address set, not application basic-block or edge coverage. MMIO instructions are
counted and skipped (aarch64 fixed width; x86_64 reports
`UnsupportedMmioSkip`). It is a validation reference for small bare-metal
workloads. Compose campaigns use a low-overhead alternative: each vCPU records
PCs at deterministic handled-exit intervals and pause barriers, so ordinary
devices stay enabled. Campaign plans can select that accumulated signal,
markers, or final checkpoint PCs as replay-locked coverage baselines.
At every campaign checkpoint, Theseus verifies that each paused vCPU PC is in
its own accumulated sample set. This checks the fast collector's pause-barrier
contract on real KVM without pretending it is an instruction trace.

For C workloads, the packaged `theseus-coverage-cc` frontend enables GCC
basic-block callbacks. Instrumented commands emit
`THES:COV:v1:<process>:<module>:<build-sha256>:<module-offset>` records. Theseus
deduplicates them per service, uses them when `coverage: application_blocks` is
selected, records new blocks at operation boundaries, and requires the same
sets during replay. The build digest prevents two rebuilds from being
conflated; the module-relative offset is independent of ASLR.

The packaged LLVM frontends cover C, C++, single-file Rust programs, and a
selected Cargo binary with its static Rust target dependencies. Clang uses
`trace-pc-guard`; rustc uses the corresponding `sancov-module` pass. Each
module retains at most 65,535 first-hit edges and disables additional guards.
It emits this record for every retained edge:

```text
THES:COV:v2:<process>:<module>:<build-sha256>:<edge>:<module-offset>
```

Select `coverage: application_edges` to guide a campaign with these records.
Each instrumented shared library links a hidden copy of the runtime, so a DSO
loaded with `dlopen` retains its own build, module, guard set, and address base.
The frontend can preserve an unstripped build-scoped file under `/symbols`;
`theseus-coverage-inspect` checks the sanitizer guards and GNU build ID, then
uses a module-relative offset to show its function and source line. A service's
`x-theseus.coverage` list associates each manifest with its symbol directory.
Planning validates their identities and locks both files; the Linux runner
revalidates the ELF guards, callback, build ID, and debug data before boot.
Campaign results and reports then attach functions and source lines to matching
service/process/module/build records automatically. Transparent instrumentation
of existing images is not implemented. `theseus coverage cargo` resolves one
binary's Cargo graph, hashes its package inputs, instruments target libraries
through Cargo's rustc-wrapper boundary, and links one runtime into the final
PIE. Host build dependencies and proc macros remain ordinary host tools;
`dylib` and `cdylib` targets require their own module identity.

`theseus coverage go` builds one Linux Go command with CGO disabled and adds a
first-hit callback to every block in each imported source package inside the
main module. It works on an isolated module copy, hashes the selected dependency
graph and module contents, and emits v1 application-block records. Unlike the C
v1 frontend, the final field is an absolute program counter; the build is a
fixed-address ELF, so the locked symbol file resolves it directly. External
module packages are hashed but not instrumented. Compose accepts the Go
manifest in the same service coverage catalog, validates the fixed executable,
callback symbols, build identity, and debug data before boot, and joins source
locations into the report. See Tutorial 39.

## Bounded C thread scheduling

The packaged `theseus-schedule-cc` frontend compiles a GCC C pthread program
with application basic-block scheduling points. Set `thread_schedule` on a
Compose shell operation to a repeating sequence of thread identities; Theseus
locks it into the command environment as `THESEUS_THREAD_SCHEDULE`. Thread `0`
is the initial thread; children are numbered in `pthread_create` order. Each
emitted record has this form:

```text
THES:SCHED:v1:<process>:<module>:<build-sha256>:<decision>:<from-thread>:<runnable-mask>:<selected-thread>:<module-offset>
```

Campaign results retain the ordered records per service and operation
boundary. Replay compares runnable masks, selected threads, order, build
identity, and scheduling-point offsets. Reports show the choices beside the
operation that produced them.

An operation can instead declare a bounded search with `threads`, `period`,
and `max_switches` around each period. Planning enumerates the patterns in
declaration order, rejects a search wider than 256 patterns, and stores each as a
named operation input. The ordinary deterministic campaign scheduler explores
those cases; minimization and replay refer to the locked case rather than
regenerating it.

For feedback-driven exploration, declare `runnable_prefixes` with
`max_choices` and `max_variants`. Theseus first runs an empty prefix and picks
the lowest runnable identity after the prefix ends. It then reads the retained
runnable masks, forks alternate prefixes only for identities observed in each
mask, and repeats until the campaign or variant bound is reached. Prefix
positions count only decisions with more than one runnable thread. Results,
minimization, and replay retain the exact prefix separately from the observed
scheduling trace.

This source implementation is bounded to 32 pthreads, 8,192 decisions, 8,192
synchronization events, and 128 synchronization objects. It controls
instrumented application basic blocks, joins, default mutex locking, and
untimed condition waits, signals, and broadcasts. Stable first-use object
numbers and every synchronization transition are retained and replay-checked.
Timed waits, cancellation, semaphores, direct futex use, blocking I/O,
processes, and uninstrumented library concurrency remain unsupported. Native
amd64 and arm64 KVM evidence remains pending.
