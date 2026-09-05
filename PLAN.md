# Theseus: Deterministic Simulation Testing on a Firecracker Fork

**Goal:** fork Firecracker and turn it into a deterministic execution environment for
whole-system testing — virtual time, seeded entropy, simulated net/disk with fault
injection, a guest↔host control channel, and cheap snapshot branching (the
"multiverse"). Inspired by Antithesis's Determinator (bhyve fork), but on Linux/KVM
with a Rust codebase.

**Baseline:** `firecracker/` (Apache-2.0, commit `f3f65a3`). Analysis below is from
code inspection of that commit.

---

## 1. Codebase map (what we cloned)

| Path | Role | Relevance to us |
|---|---|---|
| `src/firecracker` | API server binary (HTTP over unix socket) + main loop | Where new config knobs (seed, fault schedule) enter |
| `src/jailer` | Isolation: chroot, cgroups, namespaces (excluded from default build) | Keep as-is; orthogonal |
| `src/vmm/src/vstate/vcpu.rs` | vCPU thread; `run()` → `run_emulation()` → `handle_kvm_exit()` | **Exit-handling hub.** MMIO read/write, FailEntry, InternalError, SystemEvent handled here |
| `src/vmm/src/arch/x86_64/vcpu.rs` | Arch exits (`IoIn`/`IoOut` → PIO bus); TSC handling | TSC work is **snapshot-scaling only** (`get/set_tsc_khz`, `KVM_CLOCK_REALTIME` on restore). Guest time today = host time |
| `src/vmm/src/vstate/vm.rs` | VM fd, guest memory registration, **userfaultfd** hooks (used for lazy snapshot restore) | uffd machinery reusable for CoW branching |
| `src/vmm/src/vstate/memory.rs` | `GuestMemoryMmap` wrappers | unsafe hotspot; our new memory tricks live here |
| `src/vmm/src/devices/virtio/` | net (host **tap**), block (host file + **io_uring**), vsock (unix socket), **rng** (`rand::fill` → host getrandom), balloon, mem, pmem | Devices are the fault-injection surface |
| `src/vmm/src/devices/legacy/` | serial, i8042, rtc_pl031 (aarch64) | PIO bus pattern to copy for our control device |
| `src/vmm/src/devices/pseudo/` | placeholder/pseudo devices | Template for a minimal custom device |
| `src/vmm/src/device_manager/` | attach/restore, MMIO + PIO buses | Device insertion point |
| `src/vmm/src/persist.rs` + `snapshot/` | `create_snapshot` / `load_snapshot`, versioned `MicrovmState`, **diff snapshots via KVM dirty bitmap** | Branching foundation — but serializes to files; no in-memory fork |
| `src/vmm/src/dumbo/` | Firecracker's own TCP stack (used by MMDS) | In-tree reference for packet handling in a simulated NIC |
| `src/vmm/src/gdb/` | gdbstub (feature-gated) | Debugging synergy later |

**`unsafe` inventory:** 479 blocks. Hotspots: `virtio/iovec.rs` (30),
`virtio/vhost_user.rs` (24), `virtio/queue.rs` (19), `io_uring/*` (~32),
`vstate/memory.rs` (14), `virtio/net/*` (~23), `cpuid` (~17). Pattern: guest-memory
access + kernel FFI + io_uring. Workspace lint `undocumented_unsafe_blocks = "warn"`
is already enabled — keep that discipline.

---

## 2. Determinism insertion points (the fork's diff surface)

Ordered by build sequence. Items 2.1–2.4 are easy wins; 2.5 (virtual time) is the
design's critical piece — settled on Track B′ (below); 2.6–2.7 are the payoff.

### 2.1 Seeded entropy — `devices/virtio/rng/device.rs`
Today: `rand::fill()` (host `getrandom`) at lines ~129–135. **Replace with a seeded
ChaCha RNG** whose seed arrives via the control channel / API. Trivial, safe Rust.
Also audit `cpu_config/x86_64/cpuid` — guest `RDRAND`/`RDSEED` bypass virtio-rng;
KVM userspace can't trap them, so v1 hides those CPUID bits (guest falls back to
virtio-rng) and documents the limitation.

### 2.1b Host-side entropy leaks — `detrng.rs` (found during Phase 5 sweep)
Upstream draws guest-visible randomness from host entropy in more places than
virtio-rng: aarch64 FDT `rng-seed`, vmgenid generation IDs, MMDS token keys,
dumbo TCP ISNs. All now route through `detrng` — a VM-owned seeded ChaCha
stream, entered at root construction, child restore, and timeline execution.
Also de-randomized the test harness: descriptor-gap injection and
frame/payload generators in `test_utils`/`tap`/`block` tests and the APIC
interrupt test used unseeded `vmm_sys_util::rand` — now deterministic patterns.
Parallel in-process timelines therefore cannot interleave guest-visible host
randomness.

### 2.2 Control channel (our `VMCALL` equivalent)
- **Custom MMIO device** (`devices/pseudo/theseus.rs`): magic ("THES"),
  status, host→guest event FIFO, and guest→host command/log registers. No
  IRQ — the guest polls, the orchestrator drives host-side.
  **Simplified during implementation: MMIO on both arches** (one code path,
  boot-timer precedent) at a fixed platform slot (`THESEUS_MEM_START` =
  `0x40003000` on aarch64; after boot-timer slot on x86_64; the dynamic
  virtio MMIO base moved one slot up on both). Always attached at build.
  Not snapshotted (transient FIFO; orchestrator re-drives per branch).
  Verified through the host-side MMIO bus on real KVM
  (`device_manager::tests::test_theseus_mmio_roundtrip`).
Every control-channel read is a **branch point** in the timeline tree — log them.

### 2.3 Simulated network — `devices/virtio/net/`
Today the TX/RX path terminates in a host tap fd (`tap.rs`). **Swap the backend**:
keep the virtio frontend (guest sees a normal NIC), replace tap with an in-process
deterministic switch: delivery order, latency, partitions, packet loss driven by the
seeded RNG + fault schedule. All safe Rust on the data path except existing
iovec/queue plumbing. `dumbo/` is the in-tree packet-handling reference.

### 2.4 Simulated block device — `devices/virtio/block/`
Today: host file + io_uring (nondeterministic completion timing). v1: synchronous
in-process engine behind the virtio frontend with injected faults (latency, errors,
torn writes). Skip io_uring in the sim path; determinism beats IOPS here.

### 2.5 Virtual time — Track B′: tick-stepped clock, pure userspace
Constraint found in analysis: **KVM's userspace API cannot trap `RDTSC`** (no VMX
RDTSC-exiting knob), and the kvmclock pvclock page is maintained by KVM from the
host clock. Firecracker today only *scales* TSC frequency on snapshot restore
(`KVM_SET_TSC_KHZ`) and optionally sets `KVM_CLOCK_REALTIME` via `KVM_SET_CLOCK` —
guest time = host time. Antithesis could virtualize time fully only because they own
the whole hypervisor. So our design is:

**Run the vCPU in bounded quanta. On each quantum boundary, advance the guest
clock by a fixed simulated tick.** Key refinement made during implementation:
quanta are **exit-counted, not host-timed** — a quantum ends after N
guest-visible exits, counted in the vCPU thread. A host-time ticker would make
tick boundaries nondeterministic relative to guest execution (host scheduling
jitter); exit counting makes tick interleaving identical on every replay,
since every guest-visible event flows through exits we already handle. On each
boundary (between `KVM_RUN` calls — the only race-free moment), the vCPU
thread advances `VirtualClock` and applies it via the uniform
`KvmVcpu::apply_virtual_time(now_ns)`:
- **x86_64**: write TSC MSR (`set_tsc`); kvmclock anchored once at boot via
  `KvmVm::set_virtual_clock_ns(0)`.
- **aarch64**: write `KVM_REG_ARM_TIMER_CNT` via `KVM_SET_ONE_REG`
  (re-anchors the guest CNTVCT offset). Hard-won UAPI notes, from kernel
  source: the per-vcpu `KVM_ARM_VCPU_TIMER_CTRL` group only handles
  TIMER_IRQ_* attrs (TIMER_OFF → ENXIO); the VM-scoped
  `KVM_VM_SET_COUNTER_OFFSET` EBUSYs once vCPUs run; and the write must happen
  after `KVM_ARM_VCPU_INIT`, so the aarch64 anchor is applied by the vCPU
  thread on first run (`vclock_anchored`).
Deterministic at tick granularity; the counter free-runs only *within* a
quantum (guest counter reads don't exit — accepted, documented leak; measured
on metal as ≤ a few ticks of jitter from host preemption). Zero kernel work;
no extra threads; no cross-thread `unsafe`. **Proven on aarch64 metal:**
`test_guest_virtual_time_is_reproducible` boots a bare-metal guest that prints
CNTVCT — anchored near zero and bounded-close across runs with virtual time
on, divergent with it off. Rate limiters (host timerfds) rejected in
deterministic mode.

**Snapshot interaction (B′ + branching compose cleanly):** the virtual clock is VM
state → save `virtual_now` in `MicrovmState`; on restore `KVM_SET_CLOCK` **without**
`KVM_CLOCK_REALTIME` (that flag re-anchors to host wall time — the opposite of what
we want); keep TSC consistent with kvmclock (TSC MSR restore ordering +
`fix_zero_tsc_deadline_msr` already exist); keep `tsc_khz` constant across restores.
Synergy: B′'s quanta are deterministic pause points — exactly where snapshots/forks
should happen; branches forked from one snapshot inherit identical clock state and
diverge only via seed.

**Validation:** during Phases 1–2, log host-TSC delta per virtual tick and measure
whether time-nondeterminism actually correlates with replay divergence. Escalate
only if divergence persists in practice.

**Rejected / parked alternatives:**
- *Host time (do nothing)* — kills replay for time-sensitive logic (timeouts,
  leader election); Phase 0 placeholder only.
- *Guest-cooperative clock (SDK/faketime)* — leaks for uncooperative code; kept as
  an optional complement for clock-reading app code, not as the mechanism.
- *Out-of-tree KVM patch (RDTSC exiting, pinned pvclock)* — Antithesis-grade,
  instruction-level fidelity, but months of kernel engineering; the B′ quanta
  machinery is exactly what it would plug into, so the option stays open.
- *Linux time namespaces (`CLONE_NEWTIME`)* — dead end: fixed offsets for
  MONOTONIC/BOOTTIME only, designed for CRIU on host processes; invisible to KVM
  guests (guest time comes from TSC + kvmclock, outside any namespace).

### 2.6 Branching / multiverse — `branch.rs`, `persist.rs`
**Implemented:** pause → `BranchPoint::capture` (MicrovmState bytes + guest RAM
dump to memfd) → children restore through the existing snapshot path with the
memfd as `/proc/self/fd/<n>`. Discovered during implementation: the snapshot-file
memory path already maps `MAP_PRIVATE`, so **children get kernel copy-on-write
for free** — no uffd write-protect layer needed (sibling isolation proven by
`test_branch_children_memory_is_cow`). The only eager cost is one RAM dump per
branch point, not per child; a `clone`-style shared-dump optimization is future
work if profiling demands it. No new `unsafe` was needed after all.

### 2.7 Orchestrator — `orchestrator/` (in the vmm crate)
Tree + spawn + live explore loop, all proven on aarch64 KVM (see Phase 5).
**Parallel fan-out landed**: children of a node run on scoped threads (one
timeline per thread), results joined in spawn order so the tree stays
deterministic. Finding: `event_manager::EventManager` is not `Send`
(subscriber trait objects aren't), so capture is headless — vCPU threads
handle MMIO synchronously and pause/probe/capture are `Vmm` methods.
Constraint: parallel timelines must be free of host-fd-backed devices (tap,
file-backed block); sim backends and the MMIO door are pump-free.
**Guest SDK landed**: `src/theseus-sdk` (no_std, shared by host device and
guest code — single source of truth for registers/commands/markers), and a
Rust bare-metal guest (`mock_resources/theseus_guest_rs/`, built by its
`build.sh` into a flat arm64 Image) whose event handling branches on input
bytes — so input schedules drive divergent marker streams (proven in
`test_explore_with_rust_guest`). **True code coverage landed**:
`coverage.rs` single-steps the vCPU (`KVM_GUESTDBG_SINGLESTEP`) collecting
executed guest PCs — zero guest instrumentation; MMIO instructions are
counted and skipped (aarch64 fixed width; x86_64 errors honestly —
variable-length skip unimplemented). Proven on metal: replay-identical
coverage sets, divergent guests diverge. It is the ground-truth signal for
small workloads and the validation reference for a future fast
instrumentor. Remaining: the fast instrumentor (coverage.rs is its
  validation reference).
  **Linux-guest SDK transport landed**: `theseus_sdk::linux::TtyChannel` —
  the control channel over the serial console (`THES:M:xx` markers out,
  `THES:E:xx` events in) — no guest driver, works with stock kernels. e2e
  proven with `e2e/agent` (static musl init binary): handshake-then-events
  (input before guest UART init is dropped — the ready-marker handshake is
  the protocol fix, not a workaround).

---

## 3. Roadmap

- **Phase 0 — Baseline.** Linux + KVM host (bare metal; macOS dev machine can only
  `cargo check --target x86_64-unknown-linux-gnu`). Build Firecracker, boot a guest,
  run its test suite. Set up remote metal or a KVM-capable VM for the dev loop.
- **Phase 1 — Door + seed.** Seeded virtio-rng (2.1), PIO control device (2.2a),
  event-log skeleton. Proves guest↔host channel end-to-end.
  **Status: DONE — including the first on-metal e2e proof.** `e2e/run.sh`
  boots a real microVM (aarch64 KVM in the dev container, CI kernel +
  custom initramfs) three times: seed 42 twice, seed 1337 once.
  **Superseded by the standard-random-device work:** on aarch64, the
  Theseus kernel module consumes the FDT seed before CRNG initialization, so
  `/dev/random` and `/dev/urandom` are byte-identical for the same seed and
  differ for different seeds. The module and exact matching kernel must ship
  together; this is not a generic out-of-tree kernel-module ABI. **Status:
  DONE — control channel now works on aarch64 too.** The device
  moved from x86-only PIO to **MMIO on both architectures**
  (`devices/pseudo/theseus.rs`, fixed platform slot, verified through the
  MMIO bus on real KVM **and** by a live bare-metal guest — see Phase 4).
  MAGIC register handles per-byte reads at any window offset (fixed during
  guest bring-up). Rate limiters are host-timerfd based — a determinism
  leak: `validate_deterministic_config` in builder.rs rejects rate limiters
  when virtual time is enabled. Fixed the known upstream flake:
  `test_token_bucket_auto_replenish_one` is now deterministic via the new
  `TokenBucket::auto_replenish_at(now)` seam + synthetic clock (also the
  future hook for virtual-time-driven replenishment, if rate limiters are
  ever allowed in deterministic mode).
- **Phase 2 — Simulated net.** Drop tap, deterministic switch + fault schedule (2.3).
  First "interesting" faults (partition, delay).
  **Status: DONE (device level).** `NetBackend::{Tap, Sim}` — the virtio-net
  frontend is unchanged, the backend swaps: `PUT /network-interfaces` accepts a
  `sim` object (`seed`, `loopback`, `drop_ppm`, `partitioned`). Sim backend:
  loopback FIFO, total-partition toggle, seeded per-frame drops. RX pumped
  synchronously after TX (no host fd); tap event registration skipped for sim.
  Snapshot carries the sim config; in-flight frames intentionally dropped
  (orchestrator re-drives traffic per branch). Frame *delay* deferred to
  Phase 3 (needs virtual time). Multi-VM interconnection waits for the
  orchestrator (Phase 5). Unit-tested on x86_64 via qemu-user; upstream
  net-suite failures in container are tap/KVM-absent only (verified vs.
  baseline).
- **Phase 3 — Virtual time (B′).** Bounded quanta + tick-stepped kvmclock/TSC (2.5).
  Validate replay using the tick-delta instrumentation; escalate to the KVM patch
  only if measured divergence demands it.
  **Status: PLUMBED END-TO-END (unit-verified).** `vstate/vclock.rs`
  (`VirtualClock`, tested); KVM wrappers (`KvmVm::set_virtual_clock_ns`,
  `KvmVcpu::set_tsc`); **exit-counted quanta** in the vCPU run loop
  (`maybe_tick` on every handled exit — no ticker thread; see 2.5 for why);
  config: `machine-config.virtual_time = {tick_ns, exits_per_tick}` (default
  1ms / 1024 exits); anchoring at VM build; bookkeeping saved/restored in
  `VcpuState.vclock`/`vclock_exits` (guest-visible clock rides existing TSC
  MSR + kvmclock snapshot paths). Remaining (needs `/dev/kvm`): boot a guest
  and measure replay divergence; validate TSC-write/kvmclock consistency and
  TSC-deadline timer behavior on metal; disable/quantize host timerfds (rate
  limiters) in deterministic mode.
- **Phase 4 — First branch.** memfd snapshot + uffd CoW: one VM forks into 2
  timelines with different seeds (2.6). The minimal multiverse.
  **Status: PROVEN ON METAL (aarch64 KVM).** `branch.rs` `BranchPoint::capture`
  + `orchestrator::spawn_child`, proven by a live test
  (`orchestrator::spawn::tests::test_branch_children_diverge_only_by_seed`):
  boot parent (entropy seed 42) → pause → capture (MicrovmState + memfd RAM)
  → spawn two children → assert each child's entropy stream equals a fresh
  ChaCha stream of its derived child seed (and they differ from each other)
  → both resume and run cleanly. Eager RAM copy per branch; uffd/MAP_PRIVATE
  CoW remains the optimization.
- **Phase 5 — Orchestrator.** N-core fleet, branch tree, coverage-guided search (2.7).
  **Status: LIVE LOOP PROVEN ON METAL (aarch64 KVM), WITH A REACTIVE GUEST.**
  `orchestrator/explorer.rs`: `Explorer::explore` — boot root (seeded
  entropy), run, push control-channel events, pause, capture branch point
  with an **entropy probe** (per-node replay fingerprint), spawn children,
  recurse DFS. `test_explore_is_deterministic` runs the whole loop twice and
  asserts identical tree shape, seeds, and probes at every node, and that
  each child's probe equals a fresh ChaCha stream of its own seed.
  `test_explore_with_reactive_guest` drives the bare-metal Theseus guest
  through a rendezvous protocol (guest: boot marker → setup-complete →
  event-echo loop with 0x00 terminator + 0xFF done marker per round, looping
  forever so branches resume into the wait state): root markers =
  [0x42, events, 0xFF], child markers = [events, suffix, 0xFF], fully
  deterministic across runs.   Protocol lesson encoded: branch suffixes start
  at 1 because 0x00 is the terminator. Fault injection as a second branch
  axis: `FaultStrategy` + `spawn_child` overrides sim-net config in the
  captured state (proven: per-child drop_ppm/partition, deterministic).
  Dirty-page fingerprints (`Vmm::dirty_page_count`, KVM dirty bitmap) are a
  third replay fingerprint — memory-footprint coverage, proven deterministic
  across runs.   Novelty-guided expansion order
  (marker novelty, then seed) implemented. **True code coverage landed**:
  `coverage.rs` single-steps the vCPU (`KVM_GUESTDBG_SINGLESTEP`) collecting
  executed guest PCs — zero guest instrumentation; MMIO instructions are
  counted and skipped (aarch64 fixed width; x86_64 errors honestly —
  variable-length skip unimplemented). Proven on metal: replay-identical
  coverage sets, divergent guests diverge. It is the ground-truth signal for
  small workloads and the validation reference for a future fast
  instrumentor. The guest SDK and Linux serial transport are now available;
  parallel fan-out is implemented as scoped threads. Remaining: turn this
  library machinery into a stable user-facing test runner.

### 3.1 Productization roadmap

The core primitives above are deliberately separate from the product layer.
Early PRs established those seams. Product work now lands as complete vertical
slices: a user-facing workflow, locked artifacts, replay, reduction, report,
and a runnable self-contained tutorial together. Every PR below must have a
runnable example and acceptance tests.

| Order | One PR | Delivers | Explicitly does not deliver |
|---|---|---|---|
| P6.1 | **`cli: add Theseus test manifest v1`** | **DONE (PR #14).** A published `theseus` CLI; `theseus validate` and `theseus test --dry-run`; a versioned, self-contained `theseus.toml` that resolves artifact paths relative to its directory and produces a canonical run plan (kernel/initramfs, seed, virtual-time settings, events, and simulated-network settings). | Compose/Kubernetes, property evaluation, UI, or automatic input generation. A dry run must never require KVM. |
| P6.2 | **`cli: execute and replay one timeline`** | **DONE (PR #15).** `theseus test` launches one Firecracker timeline from that manifest, records its immutable replay bundle (artifact digests, resolved config, seed, events, faults, guest serial log), and `theseus replay <bundle>` reruns it. | Branch fan-out, minimization, or a distributed topology. |
| P6.3 | **`checks: add built-in and custom properties`** | **DONE (PR #16).** Explicit test outcomes: no guest crash, bounded completion/liveness, serial/marker expectations, and named user checks. Results are part of the replay bundle. | Assertion cataloging across languages or a hosted reporting service. |
| P6.4 | **`runner: add Docker Compose topology`** | **DONE (PR #17).** A small, documented Compose subset mapped to deterministic Theseus guests and simulated links, with per-service logs and artifact locking. | Kubernetes and unrestricted Docker compatibility. |
| P6.5 | **`faults: add lifecycle and clock schedules`** | **DONE (PR #18).** Declarative, replayable service pause/restart and virtual-clock-jump schedules, scoped to one service. | Storage corruption/torn writes, arbitrary host process faults, or thread scheduling controls. |
| P6.6 | **`faults: add deterministic storage faults`** | **DONE (PR #19).** The simulated block backend exposes errors, latency, torn writes, and corrupt reads through the same manifest/replay format. | Real host-disk fault injection. |
| P6.7 | **`explorer: make search guidance product-facing`** | **DONE (PR #20).** Branch budgets, coverage/marker novelty controls, failure preservation, and deterministic test reports through the CLI. | RL training infrastructure or a graphical multiverse debugger. |
| P6.8 | **`reports: add local timeline inspection`** | **DONE (PR #21).** A local static report with timeline tree, faults, logs, checks, coverage summaries, and copy-paste replay commands. | A hosted multi-user UI or causality analysis equivalent. |
| P6.9 | **`replay: make every result bundle self-contained`** | **DONE (PR #22).** Locked Compose and exploration bundles replay through the CLI without their source Compose file or manifest. | Cross-version replay guarantees or search minimization. |
| P7.0 | **`entropy: isolate host randomness per timeline`** | **DONE (PR #23).** VM-owned host-side ChaCha streams across parallel exploration timelines. | A distributed coordinator or randomness outside Theseus VM execution. |
| P7.1 | **`explorer: evaluate marker properties per timeline`** | **DONE (PR #24).** Marker pass/fail properties for every captured timeline, retained in locked exploration results and reports. | Serial-log properties in the headless explorer, automatic minimization, or a fast code-coverage instrumentor. |
| P7.2 | **`replay: reproduce one exploration timeline`** | **DONE (PR #25).** Replay one recorded root-to-node seed path without rebuilding its sibling subtrees. | Automatic minimization, snapshot export, or a fast code-coverage instrumentor. |
| P7.3 | **`minimize: reduce a failing exploration path`** | **DONE (PR #26).** A locked, deterministic 1-minimal `explore.events` sequence preserving the same named failed properties. | Global minimization, seed/fault minimization, or a fast code-coverage instrumentor. |
| P7.4 | **`snapshot: export one exploration timeline`** | **DONE (PR #27).** Export one recorded seed path as a self-contained Firecracker state-and-memory snapshot with locked-artifact provenance and node fingerprints. | Snapshot loading or mutation, a debugger UI, serial-log collection, or a fast code-coverage instrumentor. |
| P7.5 | **`replay: verify one exploration timeline`** | **DONE (PR #28).** Compare a targeted replay's recorded entropy, marker, and dirty-page fingerprints before accepting it as reproduced. | Whole-tree comparison, cross-version replay guarantees, serial-log collection, or a fast code-coverage instrumentor. |
| P7.6 | **`explorer: capture serial logs per timeline`** | **DONE (PR #29).** Per-seed serial logs in exploration bundles and serial properties evaluated across every captured timeline. | Serial input events, whole-tree replay verification, or a fast code-coverage instrumentor. |
| P7.7 | **`replay: verify the complete exploration tree`** | **DONE (PR #30).** Rebuild a locked exploration and compare every recorded seed path and fingerprint, rejecting any shape or behavior change. | Cross-version replay guarantees, serial-input replay, or a fast code-coverage instrumentor. |
| P7.8 | **`replay: fingerprint exploration serial logs`** | **DONE (PR #31).** Per-timeline serial-log digests are part of targeted and whole-tree replay verification when a bundle has serial logs. | Serial-input replay, cross-version replay guarantees, or a fast code-coverage instrumentor. |
| P7.9 | **`explorer: replay deterministic serial input`** | **DONE (PR #32).** Manifest `[[events]]` are injected directly into each timeline's emulated UART after its SDK rendezvous, so root and child timelines receive the same deterministic serial input without sharing host stdin. | Arbitrary serial schedules, input before SDK rendezvous, or exploration of a guest without the SDK control-channel protocol. |
| P8.0 | **`replay: pin the exploration executor`** | **DONE (PR #33).** Lock the published `theseus-explorer` binary, including its digest, into every new exploration bundle; replay, minimization, and snapshot export use that locked executor instead of silently using a newer installed one. | Compose-runner pinning, arbitrary backwards compatibility, or execution of legacy bundles without their original runtime. |
| P8.1 | **`replay: pin the topology executor`** | **DONE (PR #34).** Lock the published `theseus-topology` binary into every new Compose bundle and use its verified bundle-local copy for replay. | Cross-version guarantees for other executors or legacy bundles without their original runtime. |
| P8.2 | **`topology: inject deterministic serial input`** | **DONE (PR #35).** Deliver each Compose service's manifest `[[events]]` directly to its VM-local UART, including deterministic restarts. | Cross-service input schedules or input after a guest-controlled ready handshake. |
| P8.3 | **`topology: wait for serial readiness`** | **DONE (PR #36).** Deliver Compose serial events only after each service emits the standard `THES:M:42` ready marker; include a runnable UART-input topology tutorial. | Arbitrary later input schedules or a new guest protocol. |
| P8.4 | **`replay: verify Compose serial logs`** | **DONE (PR #37).** Record SHA-256 digests for every service serial log and reject a Compose replay when any rerun log differs from the original bundle. | Cross-version replay guarantees, non-serial service-state fingerprints, or partial-log comparison. |
| P8.5 | **`replay: verify Compose fault application`** | **DONE (PR #38).** Record a per-service SHA-256 fingerprint of applied lifecycle and clock faults, then reject a replay if the applied sequence changes. | Cross-version replay guarantees, network or storage state fingerprints, or partial fault comparison. |
| P8.6 | **`replay: verify Compose network topology`** | **DONE (PR #39).** Record the sorted simulated-switch port membership and reject a replay when the instantiated deterministic network topology changes. | Packet-level traffic fingerprints, storage state fingerprints, or cross-version replay guarantees. |
| P8.7 | **`replay: verify Compose storage state`** | **DONE (PR #40).** Record every simulated drive's final SHA-256 digest and reject a replay when its guest-written storage bytes differ. | Packet-level traffic fingerprints, cross-version replay guarantees, or host-file-backed storage. |
| P8.8 | **`replay: verify Compose network traffic`** | **DONE (PR #41).** Record deterministic per-service simulated-NIC TX/RX/drop counters, including planned restarts, and reject a replay when the traffic changes. | Packet payload capture, packet-by-packet traces, or cross-version replay guarantees. |
| P8.9 | **`network: add deterministic frame delay`** | **DONE (PR #42).** Delay each simulated frame by a configured number of scheduler rounds, shared consistently by all services in a topology. | Wall-clock timers, packet reordering, bandwidth limits, or jitter. |
| P8.10 | **`replay: verify Compose virtual time`** | **DONE (PR #43).** Record each service's final per-vCPU virtual-clock values and reject a replay when they differ. | Instruction-level clock virtualization, wall-clock behavior, or cross-version replay guarantees. |
| P8.11 | **`network: add deterministic frame jitter`** | **DONE (PR #44).** Add a seeded, per-frame scheduler-round delay that can reorder simulated-network delivery without using host time. | Wall-clock timers, bandwidth limits, or random host scheduling. |
| P8.12 | **`network: add deterministic bandwidth`** | **DONE (PR #45).** Limit each simulated NIC's outbound bytes per scheduler round with a deterministic transmit queue. | Host-time rate limiters, congestion control, or packet fragmentation. |
| P8.13 | **`network: add deterministic frame duplication`** | **DONE (PR #46).** Duplicate selected simulated frames from a seeded per-frame stream, using the same bandwidth and delivery queue as their originals. | Packet corruption, protocol-aware faults, or host traffic. |
| P8.14 | **`replay: fingerprint Compose network payloads`** | **DONE (PR #47).** Record length-delimited SHA-256 fingerprints of simulated NIC TX/RX frame streams and reject a replay when content changes at the same traffic volume. | Packet capture export, packet corruption, or cross-version replay guarantees. |
| P8.15 | **`network: add deterministic frame corruption`** | **DONE (PR #48).** Select nonempty simulated frames from a seeded per-frame stream and flip one bit before link delivery, recording corruption counts in Compose replay traffic. | Packet capture export, protocol-aware faults, or host traffic. |
| P8.16 | **`network: export deterministic Compose frame traces`** | **DONE (PR #49).** Record the first 64 TX/RX frames per simulated NIC, with scheduler round and payload hex, in each Compose service result. | PCAP compatibility, unbounded capture, or host traffic. |
| P8.17 | **`network: add deterministic MTU drops`** | **DONE (PR #50).** Drop simulated frames larger than an explicit per-NIC MTU before they enter the link. | Fragmentation, PMTU discovery, or host traffic. |
| P8.18 | **`network: bound deterministic transmit queues`** | **DONE (PR #51).** Drop simulated frames when an explicit per-NIC outbound queue limit is full. | TCP congestion control, packet fragmentation, or host traffic. |
| P8.19 | **`network: trace deterministic drops`** | **DONE (PR #52).** Export bounded simulated-NIC drop frames with the reason they were discarded. | PCAP compatibility, unbounded capture, or host traffic. |
| P8.20 | **`network: bound deterministic receive queues`** | **DONE (PR #53).** Drop simulated frames when an explicit per-NIC receive queue limit is full. | TCP congestion control, packet fragmentation, or host traffic. |
| P9.0 | **`product: autonomous Compose campaigns`** | **DONE (PR #54).** `theseus compose explore` drives a designated service through UART operation barriers, combines bounded operation histories with lifecycle/clock fault candidates, evaluates `always`/`sometimes`/`reachable`/`unreachable` serial properties across retained topology timelines, reports marker novelty, and reduces an individual violation to one self-contained Compose replay bundle. Includes a three-service no-SDK tutorial. | Dynamic topology actions, whole-topology snapshots, instruction-exact time, or large-scale RL guidance. |
| P9.1 | **`product: barrier-triggered topology faults`** | **DONE (PR #55).** Campaign candidates can `partition` or `heal` every simulated NIC on one named Compose network, or apply deterministic error/latency/torn-write/read-corruption settings to one simulated drive, immediately after a named UART operation checkpoint. Action history is retained in campaign results, static replay plans, minimization, and replay comparison; tutorial 10 uses both forms. | Directed-link rules, whole-topology snapshots, instruction-exact time, or large-scale RL guidance. |
| P9.2 | **`product: directed Compose link faults`** | **DONE (PR #56).** Campaign candidates can `link_partition` or `link_heal` one source-to-destination service path on a named simulated network after an operation barrier. The switch drops only the selected direction, records `link_partition` frame traces, and carries action evidence through reports, minimization, and replay verification. | Fault sequences, whole-topology snapshots, packet-match rules, storage recovery actions, instruction-exact time, or large-scale RL guidance. |
| P9.3 | **`product: bounded campaign fault sequences`** | **DONE (PR #57).** Campaigns combine compatible candidates in stable declaration order, up to `max_faults_per_run` (default 2; cap 4) and then apply `max_runs` to the complete corpus. Campaign results, reports, minimization, and legacy single-fault bundles all preserve or read the full sequence. | Copy-on-write whole-topology VM snapshots, unconstrained/coverage-guided sequence search, packet-match rules, storage recovery actions, instruction-exact time, or large-scale RL guidance. |
| P9.4 | **`product: barrier-triggered packet conditions`** | **DONE (PR #58).** Campaign candidates can set selected simulated-network drop, delay, jitter, duplication, corruption, bandwidth, MTU, and queue conditions after a UART operation barrier, then restore each service's declared conditions with `network_recover`. Applied action history stays in campaign reports, minimization, and replay verification. | Packet-match rules, copy-on-write whole-topology VM snapshots, unconstrained/coverage-guided sequence search, storage recovery actions, instruction-exact time, or large-scale RL guidance. |
| P9.5 | **`product: barrier-triggered storage recovery`** | **DONE (PR #59).** Campaign candidates can restore one simulated drive's declared error, latency, torn-write, and read-corruption settings with `storage_recover`, while retaining its guest-written bytes, queued work, and seeded I/O stream. Applied action history stays in campaign reports, minimization, and replay verification. | Packet-match rules, copy-on-write whole-topology VM snapshots, unconstrained/coverage-guided sequence search, instruction-exact time, or large-scale RL guidance. |
| P9.6 | **`product: EtherType-matched campaign packet loss`** | **DONE (PR #60).** Campaign candidates can apply deterministic packet loss only to Ethernet frames matching one declared EtherType, then remove that rule with `packet_recover` without disturbing ordinary packet conditions, partitions, links, traffic evidence, queues, or seeded state. Actions stay in reports, minimization, and replay verification. | Directed packet-match rules, IP/TCP/payload filters, copy-on-write whole-topology VM snapshots, unconstrained/coverage-guided sequence search, instruction-exact time, or large-scale RL guidance. |
| P9.7 | **`product: directed EtherType campaign packet loss`** | **DONE (PR #61).** `packet_fault` and `packet_recover` can target one source-to-destination service path with `from` and `to`. The switch gives every rule an independent seed-derived decision stream, records selected drops, and leaves other paths and packet rules intact. | IP/TCP/payload filters, copy-on-write whole-topology VM snapshots, unconstrained/coverage-guided sequence search, instruction-exact time, or large-scale RL guidance. |
| P9.8 | **`product: IPv4 transport campaign packet loss`** | **DONE (PR #62).** Packet campaigns can match IPv4 protocol and TCP/UDP source or destination ports, with narrower selectors taking precedence over broad EtherType rules. | IPv6, payload filters, copy-on-write whole-topology VM snapshots, or coverage-guided sequence search. |
| P9.9 | **`product: IPv6 transport campaign packet loss`** | **DONE (PR #63).** Packet campaigns apply the same protocol and TCP/UDP port selectors to IPv6 Ethernet frames, including directed rules and replay evidence. | IPv6 extension-header traversal, payload filters, copy-on-write whole-topology VM snapshots, or coverage-guided sequence search. |
| P10.0 | **`campaigns: restore reusable whole-topology checkpoints`** | **DONE (PR #64).** Boot and quiesce the complete Compose topology once, snapshot every Firecracker VM to immutable `MAP_PRIVATE` memory files, and restore every campaign/minimization child from that shared branch point. Preserve VM state, serial transcript prefixes, scheduler cursors, simulated-block state, simulated-NIC queues/RNG/counters, and simulated-switch queues/rules/rounds; reattach restored NICs to fresh runner-owned switches before resuming. | Prefix-tree checkpointing after arbitrary operation barriers, UFFD-backed CoW memory, cross-host snapshot compatibility, or coverage-guided campaign selection. |
| P10.1 | **`campaigns: checkpoint operation-prefix trees`** | **DONE (PR #65).** Materialize one whole-topology checkpoint after every distinct campaign operation/action prefix, then restore each schedule and minimization attempt from its nearest prefix node. Prefix keys include serial input and applied barrier actions, so faulted and unfaulted histories remain isolated. Report the tree's node and reuse counts while keeping every leaf replay bundle a normal self-contained event plan. | UFFD-backed CoW memory, cross-host snapshot compatibility, coverage-guided campaign selection, or prefix sharing across different lifecycle schedules. |
| P10.2 | **`campaigns: guide selection by serial-marker coverage`** | **DONE (PR #66).** Generate the complete bounded corpus, execute a deterministic seed leaf, then prioritize untried schedules that extend prefixes producing new UART markers or failures. Keep stable breadth-first ordering for ties, retain every selection reason in campaign reports, and preserve the full generated-candidate count. | Topology-state coverage, guest-PC coverage, probabilistic/RL guidance, UFFD-backed CoW memory, or prefix sharing across different lifecycle schedules. |
| P10.3 | **`campaigns: guide selection by topology-state coverage`** | **DONE (PR #67).** Add deterministic final-state signatures from simulated-drive digests, simulated-network traffic/payload fingerprints, and virtual clocks. Prioritize continuations that reach a previously unseen state even when UART markers are unchanged; keep serial and applied-fault evidence separate so routine output and declared faults do not create false novelty. Report state novelty per timeline and aggregate unique-state coverage. | Guest-PC coverage, probabilistic/RL guidance, UFFD-backed CoW memory, or prefix sharing across different lifecycle schedules. |
| P10.4 | **`replay: lock adaptive campaign decisions`** | **DONE (PR #68).** Record the selected operation/fault corpus order, selection reasons, marker novelty, topology-state signatures, action evidence, and final status; campaign replay restores and executes that exact recorded corpus instead of re-searching. Reject a replay when any recorded decision or coverage outcome diverges, while retaining compatibility with older bundles that lack newer evidence fields. | Guest-PC coverage, probabilistic/RL guidance, UFFD-backed CoW memory, or prefix sharing across different lifecycle schedules. |
| P10.5 | **`campaigns: minimize selected fault sequences`** | **DONE (PR #69).** After reducing a failing campaign's operation history, independently remove each selected fault while the same property still fails. Reuse the operation/action checkpoint tree for every attempt and record the original and minimized operation and fault sequences in the self-contained replay bundle. | General delta debugging, guest-PC coverage, probabilistic/RL guidance, UFFD-backed CoW memory, or prefix sharing across different lifecycle schedules. |
| P10.6 | **`campaigns: delta-debug campaign sequences`** | **DONE (PR #70).** Use coarse-to-fine contiguous deletion passes for operation and fault sequences, preserving 1-minimal counterexamples while removing long irrelevant ranges in fewer replays. Record per-pass replay counts and render the reduction evidence in the offline report. | Generalized predicates, guest-PC coverage, probabilistic/RL guidance, UFFD-backed CoW memory, or prefix sharing across different lifecycle schedules. |
| P10.7 | **`campaigns: explore bounded operation sequences`** | **CURRENT PR.** Explore every ordered UART operation history, including repetitions, through an explicit bounded depth. Keep stable breadth-first generation, fault applicability, adaptive selection, exact replay, minimization, checkpoints, and reports working over the expanded corpus. | Generalized operation grammar, guest-PC coverage, probabilistic/RL guidance, UFFD-backed CoW memory, or prefix sharing across different lifecycle schedules. |

The manifest is an execution contract, not a second
Firecracker configuration language. Keep its first version intentionally
small, reject unknown fields, resolve every relative path from the manifest
directory, and record a normalized form so the runner can execute exactly what
the manifest validation accepted. The CLI consumes released Theseus runtime artifacts; it does
not shell out to a source checkout.

## 4. Working agreements

- **License:** Apache-2.0 — fork freely; keep `NOTICE`/attribution; track upstream
  as a remote for security fixes (shallow clone now → full fetch when hacking starts).
- **Unsafe policy:** reuse existing iovec/queue/memory abstractions wherever
  possible; new `unsafe` only where measured (guest-memory fast path, uffd CoW,
  io_uring); every block documented (lint already enforces this).
- **Dev platform reality:** **KVM is available**: Docker Desktop on Apple
  Silicon exposes `/dev/kvm` (aarch64) in `--privileged` containers — the full
  vmm suite (786 tests incl. tap/network/KVM) runs green natively. x86_64-only
  paths (control channel, virtual time) cross-compile and run unit tests under
  qemu-user; their KVM ioctls still need x86 metal.
- **Scope discipline:** VMM changes minimal; exploration logic in the orchestrator
  crate; no QEMU-style feature creep (Firecracker's charter is our ally).

## 5. Key references

- Antithesis deterministic-hypervisor design (bhyve fork, virtual clock via PMC,
  VMCALL channel): antithesis.com/blog/deterministic_hypervisor/
- dhyve — open-source deterministic bhyve fork: github.com/pgraug/dhyve-src
- rust-vmm crates (if we ever need pieces Firecracker doesn't expose)
