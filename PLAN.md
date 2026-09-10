# Theseus roadmap

## Objective

Build an open, deterministic system-testing environment for Linux services.
Theseus runs a Firecracker/KVM topology, controls its nondeterministic inputs,
branches it at reproducible checkpoints, searches faulted histories, and emits
one self-contained replay bundle for every result.

The target is the useful part of the Antithesis experience: test a real service
topology, explore failures automatically, reproduce any result exactly, and
understand why that history was selected. Theseus is not yet an instruction-
perfect deterministic hypervisor or a hosted Antithesis replacement. Do not
claim either.

## Current baseline

### What is shipped

- Published macOS-arm64 and Linux amd64/arm64 CLI/runtime artifacts, plus
  Linux multi-architecture OCI images. Tutorials consume those artifacts and
  are self-contained directories.
- Seeded guest entropy, including `/dev/random` and `/dev/urandom` through the
  matching Theseus kernel/module distribution; a serial/TTY control protocol;
  and a guest SDK as an optional third integration path.
- Deterministic simulated NICs and storage: delay, jitter, partitioning,
  loss, duplication, corruption, bandwidth/queue/MTU limits, and storage
  errors, latency, torn writes, and read corruption.
- Exit-counted virtual time, deterministic topology rounds, lifecycle faults,
  causal UART barriers, and a single topology-wide budget. No normal topology
  path waits on host elapsed time.
- Compose campaigns: operation/input grammars, state and serial guards,
  cross-service JSON evidence, property checks, deterministic fault actions,
  minimization, locked replay, portable HTML/Markdown/JSON/JUnit reports.
- A whole-topology operation-prefix checkpoint tree. A shared prefix is
  captured once, then later schedules restore it instead of replaying its
  earlier operations. Marker, paused-PC, topology-state, property, adaptive,
  and posterior guidance are deterministic and replay-checked.
- Explorer primitives: in-memory branch snapshots, kernel COW child mappings,
  seeded child divergence, dirty-page and marker novelty, and slow
  single-step PC coverage as a ground-truth reference.

### Constraints we must keep visible

- KVM does not trap ordinary clock reads. Virtual time is deterministic at
  exit-counted quantum boundaries, but a guest can see a small host-time tail
  while it runs within a quantum. See `docs/determinism.md`; do not call this
  instruction-perfect replay.
- Compose checkpoint snapshots currently use Firecracker snapshot files. The
  prefix tree avoids repeated histories, but it is not yet a zero-copy,
  whole-topology snapshot store.
- Single-step coverage is intentionally slow and suitable only as a reference
  signal. Campaign PC samples are cheap checkpoint observations, not execution
  coverage.
- The supported production path is Linux/KVM. macOS is a CLI authoring
  platform, not a local VM execution platform.

## Architecture that work must preserve

```
manifest / Compose input
        ↓ normalize and lock released artifacts
deterministic topology runner
        ↓ rounds, UART, faults, simulated devices
whole-topology checkpoint tree ──→ campaign corpus scheduler
        ↓                                  ↓
locked replay bundle ← evidence, properties, minimization, report
```

The replay bundle is the product boundary. New guidance, metrics, artifacts,
or debugging data are only complete when they are copied into the bundle,
understood by offline reports, and verified on replay.

## Completed tranches

| Tranche | Status | Outcome |
| --- | --- | --- |
| P0–P5 core | Done | Deterministic devices, virtual-time plumbing, branching, explorer, serial transport, and reference coverage. |
| P6–P10 product | Done | Public CLI, Compose runner, replay/minimization, reports, tutorials, and autonomous campaigns. |
| P11 release | Done | Multi-architecture artifacts, consumer verification, provenance, SBOMs, reproducible build inputs, and external rebuild witnesses. |
| P12 campaign correctness | Done through PR #169 | Prefix checkpoints, coverage/property guidance, detailed operation evidence, deterministic lifecycle scheduling, and replay proof. |
| P13 scalable exploration | Done through PR #170 | Checkpoint economics, per-decision guidance ledgers, replay-checked search evidence, and portable reports. |

P12 and P13 are closed. Do not reopen them for isolated report fields or
scheduler bookkeeping. Fold new search capability into the next complete
tranche.

## Next work, in order

### P13 — scalable deterministic exploration

**Done in PR #170.** The checkpoint tree now records captured/restored/avoided
work, every adaptive choice retains its input ledger, and replay checks the
same search evidence that reports display.

### P14 — deterministic-runtime certification

Turn the current virtual-time caveat into a tested support contract.

**Current big PR:** turn one fixed Compose topology into a reusable,
release-attested certification witness.

- Add `theseus-topology certify`: require KVM and virtual time, execute once,
  replay once, and save a stable certificate containing exact serial, entropy,
  storage, network, virtual-clock, lifecycle, and action evidence.
- Reject host timerfd rate limiters plus tap, Unix-socket vsock,
  file-backed/vhost-user block, and host-pmem devices at the VMM boundary
  whenever virtual time is enabled.
- Ship a self-contained certification tutorial and a manually dispatched,
  native amd64/arm64 KVM-metal workflow that attests and attaches the result to
  its SHA release.
- Use the first metal certificates to measure timer-deadline and quantum-tail
  behavior. Escalate to kernel/KVM work if the exact replay witness diverges.

**Exit criteria:** supported hardware/configuration pairs have a reproducible
certification artifact; unsupported configurations are rejected or labelled
honestly. Escalate to kernel/KVM work if the quantum tail causes real replay
divergence.

### P15 — zero-copy whole-topology checkpoints

Make branching economical at the data-plane level, not just at the operation
history level.

- Replace per-prefix snapshot-file copying with a retained, shared memory
  backing and kernel COW children for every service in a topology.
- Keep switch state, UART transcripts, storage state, scheduler cursors, and
  virtual clock state atomic with the VM memory barrier.
- Bound cache lifetime and account for resident/shared/private pages without
  making host timing part of replay evidence.
- Benchmark topology fan-out by deterministic work counts and separately
  publish non-authoritative wall-clock measurements.

**Exit criteria:** sibling topology leaves share immutable memory pages; branch
isolation and replay are proven under multi-service network/storage faults;
cache reclamation cannot invalidate a locked replay bundle.

### P16 — usable execution coverage and search guidance

Replace checkpoint-PC sampling as the main coverage signal.

- Build a low-overhead coverage collector that can run with normal devices and
  captures stable execution-location identities across a complete service
  topology.
- Validate it against the existing single-step collector on small guests.
- Feed novelty into the corpus scheduler with deterministic tie breaks; retain
  the complete choice evidence and replay-check it.
- Add an evaluation workload where marker-only, checkpoint-PC, and execution
  coverage select materially different histories.

**Exit criteria:** coverage works on practical campaign workloads without
single stepping; its identity, corpus choice, and replay behavior are stable.

### P17 — ordinary workload integration

Make Theseus useful without a bespoke guest protocol.

- Define a container/service driver contract that can wrap an existing
  integration test, HTTP/gRPC client, or shell workload.
- Keep serial/TTY as a supported low-level path, but provide first-class
  readiness, operation, assertion, and correlation adapters for normal
  services.
- Package examples that require only released Theseus artifacts and their own
  tutorial directory.

**Exit criteria:** an unmodified multi-container integration workload can be
run as a deterministic campaign with properties, faults, minimization, and
replay.

### P18 — multiverse debugging

Turn a replay bundle into an investigation surface.

- Add timeline/event queries across service logs, fault actions, topology
  state, properties, and coverage.
- Support comparing a failing leaf with a passing sibling and explain their
  first causal divergence.
- Retain all debugger input in the portable bundle; do not require a hosted
  service to understand a failure.

**Exit criteria:** a user can answer “what changed before this failure?” from a
bundle without manually diffing serial logs or snapshots.

### P19 — public capability evaluation

Prove the platform on real distributed-system failures.

- Maintain a small, versioned suite of public multi-service workloads and
  seeded injected faults with expected properties.
- Report replay rate, unique states/coverage, checkpoint work, reduction
  quality, and investigation time. Compare against conventional chaos runs;
  describe Antithesis differences without unsupported claims.

**Exit criteria:** a reproducible public evaluation demonstrates bugs or
failure modes that ordinary repeated integration tests miss.

## Rules for future PRs

- Work by capability tranche, not one field or one edge case per PR.
- A product-facing change includes execution, locked replay evidence, report,
  tutorial/example, and tests in the same PR.
- Never use host wall time as a test oracle, scheduler input, or replay proof.
  Host-time metrics are optional diagnostics and must be marked as such.
- Preserve backwards reading of old replay bundles when safe; never silently
  weaken verification of a newly written bundle.
- Keep tutorials self-contained. Their directory is their working directory;
  they may depend only on published Theseus binaries or images.
