# The determinism model

A run is only useful if its recorded observations are replayable. This
document lists the inputs Theseus controls and the limits that remain. See
also [architecture.md](architecture.md).

## Controlled sources

### Entropy

Theseus seeds the entropy sources it owns:

- **virtio-rng** serves a seeded ChaCha stream (`PUT /entropy` takes
  `"seed"`; deterministic by default). The stream state is part of the
  snapshot, so a restored VM continues at the exact byte position, and
  branched children are reseeded (`Entropy::reseed`).
- **Host-side entropy** — the aarch64 FDT `rng-seed`, VM Generation IDs,
  MMDS token keys, dumbo TCP ISNs — all flow through `engine::detrng`, one
  seeded ChaCha stream per process, initialized from the run seed.

Linux also mixes guest timing into its CSPRNG. The published arm64 runtime
therefore includes a matching guest module that installs the Theseus seed into
the normal CRNG. Tutorials 1 and 2 exercise `/dev/random` and `/dev/urandom`
with that kernel/module pair. Without the module, seeded virtio entropy alone
does not guarantee identical Linux CSPRNG output.

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

Native tests can check that an enabled counter is anchored and remains near
the requested tick progression. That is not a bitwise guarantee for reads
inside a quantum.

### Network

`NetBackend::Sim` replaces the host tap behind the virtio-net frontend:
loopback, total partition, and seeded per-frame drops (`drop_ppm`).
Children of one branch point can receive different fault schedules (the
sim config is rewritten in the captured state at spawn).

### Instrumented C threads

Programs built with `theseus-schedule-cc` take an explicit repeating pthread
schedule from `THESEUS_THREAD_SCHEDULE`. The runtime permits one instrumented
application basic block at a time. It records the current thread, runnable
mask, selected thread, and build-scoped point for every choice; campaign replay
requires the same ordered records. Creation-order identities and the locked
build make this path independent of ASLR and host thread timing within its
supported boundary.

Default mutex locks use nonblocking acquisition under the same scheduler.
Mutex waiters leave the runnable mask until release. Untimed condition waits
release their mutex and leave the mask; signal selects the lowest stable
waiting identity, while broadcast makes every waiter runnable. Ordered
synchronization events use stable first-use object numbers and are retained at
operation boundaries and checked during replay.

A Compose search contract deterministically expands a declared thread set,
period, and adjacent-switch limit into at most 256 named patterns. The locked
plan contains the complete expanded patterns, so later selection and replay do
not depend on the expansion implementation.

### Everything else the guest can touch

- Rate limiters use host timerfds — **rejected** when virtual time is
  enabled (`validate_deterministic_config`).
- Tap NICs, Unix-socket vsock, file-backed or vhost-user block devices, and
  host-backed pmem are **rejected** when virtual time is enabled. A
  deterministic topology uses Theseus's simulated NIC and memory-only block
  devices instead.
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
- **Unmodified Linux CSPRNG.** A stock kernel can mix timing jitter, so its
  random-device output may diverge even when virtio entropy is seeded. Use the
  matching published kernel/module pair when random-device replay matters.
- **`detrng` owns one stream per VM timeline.** Parallel in-process timelines
  enter distinct streams, so their
  host-side random calls cannot interleave.
- **io_uring / file-backed block.** Not simulated yet; deterministic mode
  expects sim or inert storage backends.
- **Unsupported thread blocking.** The bounded GCC C scheduler controls
  application basic blocks, joins, default mutex locking, and untimed
  condition waits/signals/broadcasts. Timed waits, cancellation, semaphores,
  direct futex use, blocking syscalls, processes, and uninstrumented library
  concurrency remain outside the supported scheduling profile.

## Replay fingerprints

Each captured timeline node records:

1. **Entropy probe** — next bytes the entropy device would serve (must be
   a fresh ChaCha stream of the node's seed).
2. **Markers** — guest log bytes through the control channel (behavioral
   coverage).
3. **Dirty pages** — KVM dirty-bitmap count at capture (memory footprint).

Replay compares these fields at every retained node. A match is evidence for
those recorded observations, not a proof about unrecorded guest state.

## Certify a runtime

On a Linux host with read/write `/dev/kvm`, run a fixed Compose plan twice and
write a support-profile witness:

```sh
theseus compose plan > plan.json
theseus-topology certify --plan plan.json --output certificate
```

The second execution is a normal locked replay of the first. Its certificate
records the plan digest, platform profile, and exact serial, entropy,
storage, network, virtual-clock, lifecycle, and scheduled-action comparisons.
Certification fails closed without virtual time, KVM access, or the supported
simulated-I/O profile. It does not claim instruction-by-instruction equality
for counter reads inside a virtual-time quantum.
