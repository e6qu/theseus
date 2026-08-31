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

## Repository layout

| Path | What it is |
|---|---|
| [`firecracker/`](firecracker/) | The fork — Apache-2.0 upstream code plus marked deviations ([provenance](firecracker/README-THESEUS.md)) |
| [`sdk/`](sdk/) | `theseus-sdk` — protocol contract, bus primitives, guest transports ([README](sdk/README.md)) |
| [`engine/`](engine/) | `theseus-engine` — detrng, virtual clock, sim net, control door ([README](engine/README.md)) |
| [`orchestrator/`](orchestrator/) | `theseus-orchestrator` — branching, coverage, explorer ([README](orchestrator/README.md)) |
| [`e2e/`](e2e/) | Live-KVM end-to-end proofs ([README](e2e/README.md)) |
| [`docs/`](docs/) | Design documentation |

## Documentation

- [Architecture](docs/architecture.md) — crate layout, dependency direction, layers
- [The determinism model](docs/determinism.md) — what's closed, what's leaked, replay fingerprints
- [The control channel](docs/control-channel.md) — registers, serial transport, protocol rounds
- [Exploration](docs/exploration.md) — branch points, timeline tree, parallel explorer, coverage
- [Tutorial](docs/tutorial.md) — a full fault-hunting walkthrough (non-trivial)
- [Testing](docs/testing.md) — dev loop, e2e proofs, CI

## Quickstart

Requires a Linux+KVM host (on Apple Silicon, a privileged aarch64 Docker
container works — see [docs/testing.md](docs/testing.md)):

```sh
cd firecracker && cargo test -p vmm --lib -- --test-threads=1   # 761 tests
cargo test --manifest-path engine/Cargo.toml
cargo test --manifest-path orchestrator/Cargo.toml
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
