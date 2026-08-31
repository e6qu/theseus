# Theseus

Deterministic simulation testing for whole distributed systems, built on a
fork of [Firecracker](https://github.com/firecracker-microvm/firecracker).

Theseus runs an entire system — services, dependencies, workloads — inside a
deterministic environment where every source of nondeterminism (entropy,
time, scheduling, network faults) is controlled and seed-driven. A run is
perfectly replayable; timelines can be branched into multiverses that
diverge only by seed.

## What's here

- **`firecracker/`** — the fork (upstream base `f3f65a3`; see
  `firecracker/README-THESEUS.md` for provenance and the rationale for
  every deviation from vanilla upstream).
- **`PLAN.md`** — design notes and per-phase verification status.
- **`e2e/`** — live-KVM proofs: seed-deterministic guest entropy, the MMIO
  control channel, and the Linux guest SDK transport.

## The engine

- Seeded entropy (guest-visible, snapshot-continuous) and a seeded
  host-side RNG (`detrng`) for everything else a guest can observe.
- A control channel (guest↔host door) over MMIO and serial console, with a
  `no_std` guest SDK.
- Simulated network with deterministic drops/partitions and per-branch
  fault schedules.
- Tick-stepped virtual time (exit-counted quanta) on x86_64 and aarch64.
- In-memory timeline branching with kernel copy-on-write.
- A parallel rendezvous explorer, plus ground-truth single-step coverage.

**Verification:** 802/802 tests on aarch64 KVM; e2e boot proofs in `e2e/`.

## License

Copyright 2026 Adrian Mârza
(<https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/>) and
contributors to Theseus.

Theseus is licensed under the **GNU Affero General Public License, version
3.0 or later** (`AGPL-3.0-or-later`); see `LICENSE`. Files derived from
Firecracker remain under the Apache License 2.0 — see `firecracker/LICENSE`
and `firecracker/NOTICE`.
