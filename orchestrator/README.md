# theseus-orchestrator

Timeline branching, ground-truth coverage, and the exploration engine.
Depends on `vmm` one-way (no cycles). AGPL-3.0-or-later (see
[../LICENSE](../LICENSE)).

## Modules

| Module | What it does |
|---|---|
| `branch` | `BranchPoint`: pause a microVM, capture its state in memory (serialized state + guest RAM in a memfd), and spawn children through the snapshot-restore path with kernel copy-on-write |
| `coverage` | Single-step code coverage via `KVM_GUESTDBG_SINGLESTEP` — the set of executed guest PCs with zero guest instrumentation |
| `oci` | Flatten a container image (`docker save` tar) into a bootable initramfs with the pivot init injected — container images as test targets |
| `orchestrator/tree` | `TimelineTree`: the multiverse as a data structure — deterministic DFS order, seed paths for replay |
| `orchestrator/spawn` | `spawn_child`: restore from a branch point, reseed entropy, apply per-child fault schedules |
| `orchestrator/explorer` | `Explorer`: the live loop — rendezvous protocol, parallel fan-out on scoped threads, novelty-guided expansion, per-node fingerprints |

The `pivot/` subdirectory builds the static PID-1 binary embedded in
flattened images (pivot.bin is committed prebuilt, like the test guest
images in the fork).

## Documentation

- [Exploration](../docs/exploration.md) — the multiverse machinery in depth
- [The determinism model](../docs/determinism.md) — replay fingerprints this crate produces
- [Architecture](../docs/architecture.md) — dependency direction
