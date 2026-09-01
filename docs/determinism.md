# The determinism model

A run is only useful if it is *replayable*: same seed in, same observable
behavior out. This document lists every source of nondeterminism in a
Firecracker microVM and how Theseus closes it — plus the leaks we know
about and have not closed. See also [architecture.md](architecture.md).

## Closed sources

### Entropy

Every byte a guest can observe as "random" is seeded:

- **virtio-rng** serves a seeded ChaCha stream (`PUT /entropy` takes
  `"seed"`; deterministic by default). The stream state is part of the
  snapshot, so a restored VM continues at the exact byte position, and
  branched children are reseeded (`Entropy::reseed`).
- **Host-side entropy** — the aarch64 FDT `rng-seed`, VM Generation IDs,
  MMDS token keys, dumbo TCP ISNs — all flow through `engine::detrng`, one
  seeded ChaCha stream per process, initialized from the run seed.

Proven end to end: `/dev/hwrng` is byte-identical across same-seed boots
and differs across seeds (`e2e/run.sh`).

### Time

Track B′: **tick-stepped virtual time with exit-counted quanta.**

- The vCPU runs in bounded quanta; a quantum ends after N guest-visible
  exits (`exits_per_tick`, default 1024). Quanta are exit-counted, not
  host-timed, so tick boundaries are deterministic relative to guest
  execution.
- At each boundary the vCPU thread advances the virtual clock by one tick
  (`tick_ns`, default 1 ms) and applies it: TSC MSR on x86_64,
  `KVM_REG_ARM_TIMER_CNT` (CNTVCT offset) on aarch64. The anchor is applied
  once before the first `KVM_RUN`.
- Enable with `machine-config.virtual_time`.

Proven on metal: a bare-metal guest reading CNTVCT sees anchored,
bounded-close time across runs with virtual time on, and divergent host
time with it off.

### Network

`NetBackend::Sim` replaces the host tap behind the virtio-net frontend:
loopback, total partition, and seeded per-frame drops (`drop_ppm`).
Children of one branch point can receive different fault schedules (the
sim config is rewritten in the captured state at spawn).

### Everything else the guest can touch

- Rate limiters use host timerfds — **rejected** when virtual time is
  enabled (`validate_deterministic_config`).
- The test harness itself used unseeded randomness (descriptor gaps,
  frame payloads); now fixed patterns.
- `test_token_bucket_auto_replenish_one` flaked on wall-clock sleeps; it
  now drives a synthetic clock via `TokenBucket::auto_replenish_at`.

## Known leaks (honest list)

- **Mid-quantum free-run.** Guest counter reads do not exit, so between
  ticks the counter runs at host rate, plus host preemption jitter
  (measured ≤ a few ticks). Bitwise replay of *clock reads* is Track B
  (trap counter reads — parked deliberately; on x86 it needs a KVM patch,
  on aarch64 there is no userspace trap knob).
- **Guest-internal jitter entropy.** The Linux kernel's CSPRNG mixes
  timing jitter, so `/dev/urandom` diverges even on same-seed boots. A
  hypervisor cannot close this without guest cooperation.
- **`detrng` owns one stream per VM timeline.** Parallel in-process timelines
  enter distinct streams, so their
  host-side random calls cannot interleave.
- **io_uring / file-backed block.** Not simulated yet; deterministic mode
  expects sim or inert storage backends.

## Replay fingerprints

Determinism is asserted continuously, not assumed. Each timeline node
records:

1. **Entropy probe** — next bytes the entropy device would serve (must be
   a fresh ChaCha stream of the node's seed).
2. **Markers** — guest log bytes through the control channel (behavioral
   coverage).
3. **Dirty pages** — KVM dirty-bitmap count at capture (memory footprint).

Two runs of the same exploration must produce identical fingerprints at
every node; the explorer's tests assert exactly that.
