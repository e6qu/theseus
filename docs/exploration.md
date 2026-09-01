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
timelines share all pages until they write them (proven by
`test_branch_children_memory_is_cow`). The only eager cost is one RAM dump
per branch point.

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
2. Rendezvous: wait for `SETUP_COMPLETE`, push events + terminator, wait
   for the done marker, pause, fingerprint, capture.
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

The path starts with the root seed. Theseus rejects a path that does not match
the locked branch contract. It also compares the selected timeline's entropy
probe, markers, and dirty-page count with the recorded result. A mismatch makes
the replay fail and is shown in its static report.

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

`coverage.rs` collects executed guest PCs via `KVM_GUESTDBG_SINGLESTEP` —
true coverage with zero guest instrumentation. MMIO instructions are
counted and skipped (aarch64 fixed width; x86_64 reports
`UnsupportedMmioSkip`). It is the ground-truth reference for small
workloads and for validating a future fast instrumentor; the explorer
currently uses markers and dirty pages as its cheap signals.
