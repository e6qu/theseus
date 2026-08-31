# Theseus — a deterministic-simulation fork of Firecracker

This directory is a **fork** of [Firecracker](https://github.com/firecracker-microvm/firecracker),
absorbed into the Theseus repository on 2026-08-31.

## Provenance

- **Upstream base:** `firecracker-microvm/firecracker@f3f65a3`
  ("pci: Drop the dead ACPI PCI hot-plug AML", v1.17.0-dev).
- **Fork commit:** branch `theseus` @ `d047c78`
  ("theseus: deterministic simulation testing layer"), one squashed commit
  on top of the base. The fork's `.git` was removed when absorbed; this
  document is the linkage.
- **Upstream sync:** there is no git remote. To sync, re-clone upstream and
  diff against this tree; all deviations are listed below and marked
  `Theseus` in code comments.

## Why not vanilla upstream?

Upstream Firecracker is built for *security isolation*: fast, minimal,
sandboxed microVMs. Theseus needs *reproducibility*: byte-identical replay
of an entire system. Vanilla upstream cannot provide that because
**nondeterminism is wired into every layer it owns**:

- **Entropy is host-random.** virtio-rng serves `getrandom()`. The aarch64
  FDT `rng-seed`, VM Generation IDs, MMDS token keys, and dumbo TCP initial
  sequence numbers all come from host entropy. Any of these makes two
  identical boots diverge.
- **Time is host time.** The guest's TSC (x86_64) / CNTVCT (aarch64) and
  kvmclock follow the host wall clock, so replays see different time and
  different timer-driven behavior (timeouts, elections).
- **Devices are host-fd-backed.** Tap networking and file-backed block I/O
  inherit host scheduling, latency, and failure modes — unrepeatable by
  construction.
- **There is no replay substrate.** No seeded-RNG discipline, no timeline
  branching, no coverage feedback — the things deterministic simulation
  testing is made of.
- **Even the tests are nondeterministic.** Upstream's own harness used
  unseeded randomness (descriptor gaps, frame payloads) and a wall-clock
  rate-limiter test that flaked under load.

Firecracker is the right *base* (minimal VMM, Rust, clean device model, no
QEMU legacy), but the deterministic engine had to be built into it.

## What changed, by area

### New files

| Path | Purpose |
|---|---|
| `src/vmm/src/detrng.rs` | Process-wide seeded RNG for all host-side guest-visible entropy |
| `src/vmm/src/vstate/vclock.rs` | Tick-stepped virtual clock (Track B′ core) |
| `src/vmm/src/devices/pseudo/theseus.rs` | Control-channel MMIO device (guest↔host door) |
| `src/vmm/src/devices/virtio/net/sim.rs` | Deterministic simulated network backend |
| `src/vmm/src/branch.rs` | In-memory timeline branching (memfd snapshots) |
| `src/vmm/src/coverage.rs` | Single-step code coverage (KVM_GUESTDBG) |
| `src/vmm/src/orchestrator/` | Timeline tree, child spawning, parallel rendezvous explorer |
| `src/theseus-sdk/` | `no_std` guest SDK (protocol contract, MMIO + serial transports) |
| `src/vmm/src/test_utils/mock_resources/theseus_guest*` | Bare-metal test guests (asm + Rust) and build scripts |

### Deviations from upstream

- **virtio-rng** (`devices/virtio/rng/`): host `getrandom` replaced by a
  seeded ChaCha stream; seed in `PUT /entropy` config; RNG state is
  snapshotted for stream continuity; `reseed()` for branch children.
- **Seeded host entropy**: FDT `rng-seed` (aarch64), vmgenid, MMDS token
  key, dumbo TCP ISN now draw from `detrng`, initialized from the run seed.
- **Control channel**: `TheseusDevice` on the MMIO bus at a fixed platform
  slot (both arches; the virtio MMIO base moved up one slot on each).
  Always attached, including on snapshot restore.
- **Virtual time (Track B′)**: exit-counted quanta; per-tick clock applied
  via `KvmVcpu::apply_virtual_time` — TSC MSR on x86_64,
  `KVM_REG_ARM_TIMER_CNT` on aarch64 (the per-vcpu `TIMER_OFF` attr does
  not exist in Linux; VM-scoped counter offset EBUSYs once vCPUs run).
  Config: `machine-config.virtual_time`. Rate limiters rejected in
  deterministic mode (host timerfds are a leak).
- **Simulated net**: `NetBackend::{Tap, Sim}`; tap unchanged, sim adds
  loopback/partition/seeded drops; sim config in snapshots and
  `PUT /network-interfaces` (`sim` object); per-child fault schedules
  rewrite the sim config inside captured state.
- **Branching**: `persist::restore_from_microvm_state` extracted from
  `restore_from_snapshot` (behavior-preserving refactor); branch children
  restore from memfd with `MAP_PRIVATE` (kernel CoW) and are reseeded.
- **Deterministic test harness**: descriptor-gap injection, frame/payload
  generators, and the APIC interrupt test use fixed patterns instead of
  unseeded `vmm_sys_util::rand`.
- **De-flaked test**: `test_token_bucket_auto_replenish_one` now drives a
  synthetic clock through the new `TokenBucket::auto_replenish_at(now)`
  seam (upstream's version flaked under load).
- **API/schema**: `EntropyDevice.seed`, `NetworkInterface.sim`,
  `MachineConfiguration.virtual_time` (swagger updated;
  `skip_serializing_if` keeps the JSON shape unchanged when unset).

## Verification

802/802 `vmm` lib tests pass natively on aarch64 with `/dev/kvm` (including
live multiverse branching, guest-visible entropy determinism, and
control-channel roundtrips from bare-metal and Linux guests). x86_64 code
cross-compiles and runs unit tests under qemu-user. See `../PLAN.md` for
per-phase status and the e2e harness in `../e2e/`.
