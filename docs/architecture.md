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
├── e2e/                # Live-KVM check harness (see e2e/README.md)
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
| Seeded entropy | `firecracker` (rng device) + `engine/detrng` | Supplies seeded device and host-side entropy; Linux CSPRNG replay also requires the shipped guest module. |
| Control channel | `engine/door` + `sdk` | Guest↔host door over MMIO and serial console. |
| Simulated network | `engine/simnet` | Loopback, partition, seeded drops; per-branch fault schedules. |
| Virtual time | `engine/vclock` + vCPU tick loop | Tick-stepped clock (exit-counted quanta) on x86_64 and aarch64. |
| Branching | `orchestrator/branch` | In-memory (memfd) timeline forks with kernel copy-on-write. |
| Exploration | `orchestrator/orchestrator` | Timeline tree, child spawning, parallel rendezvous explorer. |
| Execution locations | `orchestrator/coverage` | Guest-PC collection by single-step or deterministic exit sampling. |
| Application coverage | `instrumentation/c`, `instrumentation/llvm` + `cli` + `topology-runner` | GCC C blocks and LLVM C/C++/Rust edges with build-scoped, ASLR-independent identities. Compose locks LLVM manifests and symbols; the runner validates and joins them into reports for executables and DSOs. |
| Thread scheduling | `instrumentation/c` + `cli` + `topology-runner` | Bounded GCC C pthread interleavings with explicit, static, or feedback-driven choices plus replay-checked mutex and condition-variable transitions. |

## Verification model

Pull-request CI compiles the workspace and runs environment-independent tests
on an amd64 GitHub runner. KVM behavior needs separate native execution. The
manual certification workflow targets self-hosted amd64 and arm64 KVM hosts.
It stages both results, verifies their signed-release provenance and complete
archive inventories, and publishes one indexed architecture pair; only those
retained, verifiable assets are runtime evidence.

Branch capture first copies all guest RAM into a memfd. Restored children map
that memfd privately, so child writes use kernel copy-on-write. Single-step and
sampled PCs identify guest instruction addresses and remain baseline signals.
The separate C instrumentation path records application basic blocks; it does
not turn the PC collectors into block or edge coverage.

The C scheduling frontend uses the same compiler basic-block hook with a
separate runtime. It assigns stable thread identities in creation order,
serializes application blocks according to `THESEUS_THREAD_SCHEDULE`, and
emits every choice through the image pivot. The topology runner retains those
ordered records at operation boundaries and rejects a replay that changes
them. This is cooperative userspace instrumentation, not a kernel scheduler or
hypervisor-wide thread controller.
