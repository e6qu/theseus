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

Add `marker_seen` and `marker_not_seen` checks to an exploration manifest.
Their `value` is one hexadecimal byte. Each check applies to every captured
timeline, not merely the root. A failed result names the first seed paths that
violated it; replaying the locked bundle preserves the full tree and checks.
Serial-log checks are intentionally unavailable here: the headless explorer
has no serial-log transport.

## Reproduce one timeline

Every timeline in a static exploration report includes a copyable command for
its seed path. It replays the root and only the selected child at each branch;
it does not rerun siblings or the whole search tree:

```sh
theseus explore --replay exploration-dir --seed-path 42,123,456 \
  --output timeline-replay
```

The path starts with the root seed. Theseus rejects a path that does not match
the locked branch contract.

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
