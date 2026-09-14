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
(`splitmix64(base_seed ^ branch_index)` — deterministic). Fault schedules
are a second divergence axis: the sim-net config is rewritten in the
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
conflated; the module-relative offset is independent of ASLR. This bounded path
does not provide edge coverage or instrumentation for other languages.

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

This source implementation is bounded to 32 pthreads and 8,192 decisions. It
controls only instrumented application basic blocks and intercepts
`pthread_create` and `pthread_join`. A target that blocks in another
uninstrumented synchronization call can deadlock, so this is not general Linux
thread/process scheduling. Native amd64 and arm64 KVM evidence remains pending.
