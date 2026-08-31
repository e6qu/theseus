# Architecture

Theseus is a deterministic simulation engine for whole distributed systems,
built on a fork of Firecracker. This document describes the repository's
crate layout and how the pieces fit together. See also
[determinism.md](determinism.md) for the model and
[exploration.md](exploration.md) for the multiverse machinery.

## Crate layout

The license boundary *is* the directory boundary: `firecracker/` is Apache
2.0 (upstream code plus marked deviations); everything else is
AGPL-3.0-or-later.

```
theseus/
├── firecracker/        # Apache-2.0. Upstream Firecracker @ f3f65a3,
│                       # plus clearly marked deviations (see
│                       # firecracker/README-THESEUS.md)
├── sdk/                # theseus-sdk. Protocol contract + bus primitives.
│                       # no_std (bare-metal guests); `std` feature adds the
│                       # Linux serial transport and the device bus.
├── engine/             # theseus-engine. Leaf deterministic components:
│                       # detrng, virtual clock, sim net backend, control door.
├── orchestrator/       # theseus-orchestrator. Timeline branching,
│                       # coverage, and the exploration engine.
├── e2e/                # Live-KVM proof harness (see e2e/README.md)
└── docs/               # You are here.
```

Dependency direction (cycles are not allowed):

```
vmm ────────► engine ────────► sdk
 ▲            (leaf)
 │
 └── orchestrator (one-way)
```

- `vmm` (inside `firecracker/`) depends on `engine` and re-exports its
  modules at their old in-crate paths, so no fork-internal call sites
  change.
- `engine` depends only on `sdk` (never on `vmm`) — that is what makes it a
  leaf.
- `orchestrator` depends on `vmm` one-way; `vmm` does not depend on it.

## Why the split is shaped this way

`detrng`, the virtual clock, the simulated net backend, and the control
door are used *by* the VMM — they must sit below it. Branching, coverage,
and the explorer sit *above* it, orchestrating whole microVMs. The device
bus primitives (`BusDevice`, `Bus`) had to move to `sdk` so the control
door could leave `vmm` without creating a dependency cycle.

## The layers of the engine

| Layer | Where | What it does |
|---|---|---|
| Seeded entropy | `firecracker` (rng device) + `engine/detrng` | Every byte of entropy a guest can see comes from a seeded ChaCha stream. |
| Control channel | `engine/door` + `sdk` | Guest↔host door over MMIO and serial console. |
| Simulated network | `engine/simnet` | Loopback, partition, seeded drops; per-branch fault schedules. |
| Virtual time | `engine/vclock` + vCPU tick loop | Tick-stepped clock (exit-counted quanta) on x86_64 and aarch64. |
| Branching | `orchestrator/branch` | In-memory (memfd) timeline forks with kernel copy-on-write. |
| Exploration | `orchestrator/orchestrator` | Timeline tree, child spawning, parallel rendezvous explorer. |
| Coverage | `orchestrator/coverage` | Single-step PC collection — ground-truth coverage. |

## Verification model

Every layer is proven on hardware, not just written: 761 `vmm` tests plus
crate tests on aarch64 KVM, and four end-to-end boot proofs in `e2e/`. CI
(`.github/workflows/ci.yml`) runs on pull requests only and executes the
environment-independent subset on GitHub runners.
