# Theseus

**Deterministic simulation testing for whole distributed systems** — run an
entire system (services, dependencies, workloads) in an environment where
every source of nondeterminism is seeded and replayable, then fork
timelines into multiverses that diverge only by seed.

Built on a fork of [Firecracker](https://github.com/firecracker-microvm/firecracker)
(v1.17.0-dev, `f3f65a3`).

## What it does

- **Seeded everything a guest can observe**: entropy (virtio-rng, FDT
  rng-seed, vmgenid, MMDS tokens, TCP ISNs), all from one seed — so two
  boots with the same seed are byte-identical.
- **Tick-stepped virtual time** (exit-counted quanta) on x86_64 and aarch64.
- **A control channel** between guest and host (MMIO + serial console) with
  a `no_std` guest SDK.
- **Simulated network** with deterministic drops, partitions, and
  per-branch fault schedules.
- **In-memory timeline branching**: pause a VM, fork it, and run children
  that differ only by seed — with kernel copy-on-write.
- **A parallel exploration engine** that drives timelines through
  rendezvous protocols and fingerprints every node (entropy probe, markers,
  dirty pages).
- **Ground-truth coverage** via single-stepping (`KVM_GUESTDBG`).

Verified end to end: 761 `vmm` tests plus crate tests on real KVM, and four
live boot proofs in `e2e/`.

## What Theseus investigates

Theseus is built for the bugs that only appear when a system is stressed
in ways you did not plan a test for:

- **Concurrency and race conditions** — interleavings of threads and
  processes that hand-written tests never schedule.
- **Replication and consistency bugs** — split-brain, lost writes,
  divergent replicas after partitions and rejoins. For a key-value store:
  a committed write that vanishes after a failover. For a consensus
  system: two leaders elected in the same term.
- **Exactly-once violations** — a command applied twice when a retry
  meets a lost acknowledgement (the tutorial's running example), or a
  message delivered twice by a queue under reconnection.
- **Crash recovery and durability** — a database that loses its last
  write-ahead-log records when paused mid-fsync, or recovers to a torn
  state.
- **Timeout and election logic** — leader elections, lease expiry,
  failover, and retry storms under controlled virtual time.
- **Fault-handling logic** — what your code actually does on packet loss,
  partitions, node crashes, slow or corrupt storage — not what it does in
  a healthy environment.
- **Flaky tests** — failures that appear once and never again; a seed
  turns them into failures you can rerun every time.
- **Fault-schedule regressions** — behavior changes under a fixed sweep
  of drops, partitions, and seeds, compared across runs.

## Container images as test targets

Boot the artifact your CI already builds. Theseus takes a container image
(`docker save` tar), a Dockerfile (build it, then save it), or a registry
image (pull it, then save it), flattens the layers into a bootable
initramfs, injects a static pivot init that wires up the control channel,
and boots it — no guest driver, no image modification, no Dockerfile
changes. The image's entrypoint runs unchanged. See
[docs/guides/container-images/](docs/guides/container-images/).

## Requirements for the system under test

A system must satisfy the following to be tested with full replay:

1. **Boots under Firecracker.** A kernel image plus an initramfs or
   rootfs (aarch64 or x86_64). Any Linux workload; bare-metal guests work
   too.
2. **Event-driven workload.** The system takes its inputs through the
   control channel (the Theseus SDK, bare-metal MMIO or the Linux serial
   transport) rather than wall-clock sleeps or external networks.
   Behavior you want replayed must follow from events, not host time.
3. **Deterministic dependencies.** The simulated network backend is used
   for networked systems; host-fd-backed devices (tap networking,
   file-backed block storage) are outside deterministic mode. Rate
   limiters are rejected when virtual time is enabled.
4. **Optional: virtual time.** For timer-driven logic (timeouts,
   elections), enable `machine-config.virtual_time` so those decisions
   replay too.

Seeded entropy is provided by the engine itself (the entropy device and
the host-side random sources are seeded automatically from the run seed)
— it is not something you need to configure per system.

## Requirements from the user

- **Package your system** as a Firecracker-bootable image (kernel plus
  initramfs or rootfs).
- **Wire inputs through the SDK** — events in, markers out. Bare-metal
  guests use the MMIO device; Linux workloads use the serial transport.
- **Emit markers for the outcomes you care about** — one call per
  observable result. That is the entire instrumentation surface: nothing
  needs to change in your system to *run* it, only to *judge* it.
- **Choose seeds and fault schedules** for each run, or declare a Compose
  campaign: UART operations, lifecycle candidates, named-network
  `partition`/`heal` actions, directed `link_partition`/`link_heal` actions,
  simulated-drive `storage_fault` actions, and runtime properties. Theseus
  explores that deterministic corpus and reduces a violating operation history
  to one replay bundle.
- **Run on a Linux+KVM host** (on Apple Silicon, a privileged aarch64
  Docker container works).

## Repository layout

| Path | What it is |
|---|---|
| [`firecracker/`](firecracker/) | The fork — Apache-2.0 upstream code plus marked deviations ([provenance](firecracker/README-THESEUS.md)) |
| [`sdk/`](sdk/) | `theseus-sdk` — protocol contract, bus primitives, guest transports ([README](sdk/README.md)) |
| [`engine/`](engine/) | `theseus-engine` — detrng, virtual clock, sim net, control door ([README](engine/README.md)) |
| [`orchestrator/`](orchestrator/) | `theseus-orchestrator` — branching, coverage, explorer ([README](orchestrator/README.md)) |
| [`cli/`](cli/) | `theseus` — self-contained test-manifest validation and canonical dry-run plans ([README](cli/README.md)) |
| [`e2e/`](e2e/) | Live-KVM end-to-end proofs ([README](e2e/README.md)) |
| [`docs/`](docs/) | Design documentation |

## Documentation

- [Architecture](docs/architecture.md) — crate layout, dependency direction, layers
- [The determinism model](docs/determinism.md) — what's closed, what's leaked, replay fingerprints
- [The control channel](docs/control-channel.md) — registers, serial transport, protocol rounds
- [Exploration](docs/exploration.md) — branch points, timeline tree, parallel explorer, coverage
- [CLI manifest](cli/README.md) — the self-contained test-directory contract
- [Tutorials](docs/tutorials/) — hands-on walkthroughs from replay to serial input
- [Testing](docs/testing.md) — dev loop, e2e proofs, CI

## Quickstart

Requires a Linux+KVM host (on Apple Silicon, a privileged aarch64 Docker
container works — see [docs/testing.md](docs/testing.md)):

```sh
cd firecracker && cargo test -p vmm --lib -- --test-threads=1   # 761 tests
cargo test --manifest-path engine/Cargo.toml
cargo test --manifest-path orchestrator/Cargo.toml
cargo test --manifest-path cli/Cargo.toml
sh e2e/run.sh                                                    # live proofs
```

## License

Copyright 2026 Adrian Mârza
(<https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/>) and
contributors to Theseus.

Theseus-authored code (everything outside `firecracker/` that carries the
Theseus header) is licensed under **AGPL-3.0-or-later** — see
[LICENSE](LICENSE). Everything under `firecracker/` remains **Apache-2.0**
([firecracker/LICENSE](firecracker/LICENSE), `firecracker/NOTICE`), with
deviations marked in comments. The boundary is spelled out in
[firecracker/README-THESEUS.md](firecracker/README-THESEUS.md).
