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

Every successfully handled guest-visible KVM exit is appended to both a
per-vCPU rolling SHA-256 ledger and a VM-wide ledger. The record identifies the
vCPU, exit kind, and deterministic payload, including MMIO and PIO address,
width, and bytes read or written. Explicit host effects share that VM-wide
order: UART bytes, control-channel bytes, and virtual-clock jumps are recorded
in the same serialized turn in which they affect the guest. Replay checks the
expected input before delivery. A `vcpu:` or `host:` prefix identifies the
actor.

In deterministic VMs, supported userspace devices do not signal KVM through
asynchronous irqfds. They queue level or edge requests in the branch state and
wake the running vCPUs. A vCPU injects the next request with `KVM_IRQ_LINE`
before guest entry and records `vcpu:<id>:interrupt:<source>:<gsi>` in the same
machine stream. Sources cover UART, virtio MMIO, virtio MSI-X, VM generation
and clock notifications, and the i8042 keyboard. Checkpoints retain undelivered
requests, including notifications created while restoring a VM, and replay
requires the same vCPU delivery turns. Checkpoint-backed campaign replay can
materialize a recorded delivery edge after the restored device had a chance to
publish it; subsequent device reads still verify the restored state. Vhost-user
and other direct notifier paths remain outside the deterministic profile.

The VM-wide gate retains the bounded complete trace as well as the rolling
digest. Checkpoints clone both forms into each child. Fixed-run replay admits
only the actor named by every next record, checks complete writes and read
identities, and rejects an extra access at trace exhaustion. Campaign replay
uses the trace's host inputs and interrupt deliveries as its portable control
stream; intervening MMIO and PIO exits remain evidence because Linux execution
between controlled turns is not instruction-scheduled. Read values are checked
after device access and cannot be rolled back.

An attached x86 i8042 reset request ends the stream on its own recorded write.
It does not keep polling until the event loop notices an asynchronous reset
event. System reset/shutdown exits use the same terminal gate on both
architectures. Other vCPUs stop admitting effects, pending replay actors are
woken, and exit status is published before event-loop notification.

### Single-service bundles

Fresh-boot `theseus test` bundles use `theseus-replay-plan-v2` and require
`execution.json`. This file contains the full machine trace, per-vCPU ledgers,
machine ledger, observed boundary, and first active replay error. `[[events]]`
become exact `host:serial_input` decisions after the ready marker, not bytes
written to an unrecorded stdin pipe. Each event is at most 16,384 bytes,
fitting the default HTTP API limit after hexadecimal encoding.
Version-2 plans use the topology runner's `quiet loglevel=0` boot policy to
suppress host-clock-dependent kernel diagnostics. This does not control those
clocks or filter decisions out of the captured stream.
`run.entropy_device = false` omits the seeded virtio RNG for guests that do
not use it. Otherwise, kernel boot allocation can change its queue addresses
before the application starts. Replay rejects that divergence; neither the
device seed nor quiet boot makes arbitrary kernel boot deterministic.

`theseus replay --output diagnostics bundle` installs the recorded stream
before the first guest run. It checks the complete stream, local ledgers, and
terminal boundary as well as application checks. Missing or inconsistent
execution evidence is an error; deleting `result.json` cannot downgrade the
versioned replay plan. Older `theseus-run-plan-v1` bundles retain their legacy
seed/input replay behavior and do not establish machine-stream enforcement.

A host timeout pauses the VM and flushes a diagnostic cut before killing it.
Its boundary is `pause`, not `guest_exit`, and active replay rejects that cut:
the host deadline did not define a deterministic terminal decision. Runtime
errors are retained as `runtime_error` boundaries and are not replayable guest
exits either. Capturing a stream is not proof that uncontrolled execution will
reproduce it; a changed stream must fail.

Low-level API clients can `PUT /execution` before a fresh boot with
`evidence_path` and an optional `replay_trace_path` (a JSON array). The output
must not exist. `PUT /serial-input` accepts a bounded lowercase `data_hex`
payload. `PATCH /execution` with `{}` flushes a paused VM; natural exit flushes
automatically. Raw `/snapshot/load` cannot load execution capture context;
in-process topology checkpoint replay retains its existing branch-owned state.

### Ready-checkpoint bundles

Set `run.replay_start = "ready_checkpoint"` for a UART/RNG guest with virtual
time and ready-gated events. The guest must print `THES:M:42` and wait for
input. Theseus boots once, pauses at readiness, and saves a full checkpoint.
Both the first test result and every replay restore that same checkpoint.
They never use the bootstrap VM as the baseline or fall back to fresh boot.

Version-3 replay plans lock `checkpoint/metadata.json`, `vmstate`, `memory`,
and `prelude.log`, in addition to the runtime and guest inputs. Metadata binds
state/RAM hashes and lengths, machine/clock/entropy configuration, the complete
inherited trace, pending userspace interrupts, control FIFO/log, and amd64 PS/2
registers/FIFO. Loading rebuilds machine and local hashes from the validated
prefix and installs the retained host/interrupt control stream before the first
campaign resume. Fixed-run replay installs the complete expected trace.
Snapshot restore retains vCPU registers, virtual-clock counters, UART state,
and RNG state. New restore-time notifications join the retained pending queue
in the same order for the baseline and replay.

`execution.json.start` identifies the metadata digest and inherited decision
count. The prefix is captured ancestry, not actively replayed kernel boot;
only the suffix is admitted anew through guest exit. Boot output stays in
`boot/serial.log` and locked `checkpoint/prelude.log`. `serial.log` and checks
cover resumed output only. Missing members, wrong identities, changed RAM,
inconsistent origins/prefixes, and incompatible snapshots fail closed.

Low-level clients pause with `PATCH /vm`, then
`PUT /execution-checkpoint` with `{"action_type":"Create","directory":"new-directory"}`.
Load into an unconfigured API VM using `action_type: "Load"`, `directory`,
`checkpoint_sha256`, `serial_out_path`, and an `execution` object containing
`evidence_path` and optional `replay_trace_path`. Loading leaves the VM paused;
resume with `PATCH /vm` and `{"state":"Resumed"}`. Evidence output must be new;
UART output must be new or an empty regular file, not an existing log or symlink.
The origin is assigned only by verified load, not accepted from API clients.
Raw `/snapshot/load` remains separate and cannot supply execution capture.

This workflow currently excludes live block, network, vsock, pmem, balloon,
memory-hotplug, and rate-limited UART/RNG devices. It requires native Linux/KVM,
a compatible CPU/runtime, and the same architecture; RAM can contain secrets.
Kernel timers and arbitrary instruction order are still uncontrolled. A
checkpoint improves the starting state, not those guarantees. Exploration
keeps its existing branch-managed checkpoint workflow; Compose likewise manages
whole-topology checkpoints and rejects this single-service flag.

### Retained topology roots

Set `x-theseus.replay_start: ready_checkpoint` in Compose to retain the
whole-topology starting state. Every service must use virtual time and wait
for input after its readiness marker. The baseline and replay restore the same
locked state, rather than booting independently. Campaigns may reuse this root
for their operation-prefix tree; retain the complete output directory.

Ready-root exports copy locked runtime and guest inputs into their own
`checkpoint/artifacts/` directory. Replayed campaigns and minimized bundles
retain a local root with the same identity; they do not require the original
campaign directory. Campaign run subdirectories share the campaign root:
move the whole campaign, not an individual `runs/000` directory.

`theseus compose verify bundle-dir` checks ready-root integrity without KVM.
It binds configuration, members, execution ancestry, UART bytes, and complete
machine/vCPU ledgers, including their campaign aggregates. It does not decode
KVM CPU state or establish native execution provenance. Active replay remains
a separate runtime operation.

The operation-prefix cache uses a deterministic LRU policy with a 512 MiB
materialized-RAM budget. Evicted prefixes can be reconstructed from retained
ancestors. The immutable root, active working branches, binary context, and
other host allocations are outside that budget; this is not a process-RSS cap.
Reports distinguish prefix snapshot-file bytes from durable root exports and
show prefix evictions explicitly.

`starting_checkpoint` locks `metadata.json`; metadata locks every service's
`vmstate` and `memory` plus the bounded binary `context.bin`. Context contains
simulated NIC queues, seeded link state and framed digest input, switch queues,
UART transcripts, scheduler state, control/PS/2 state, execution prefixes, and
undelivered userspace interrupts. Loading checks architecture, VM configuration,
member hashes and lengths, and reconstructs rolling ledgers before creating VMs.
Root loading copies RAM into a sealed memfd and verifies the copied bytes,
so later changes to the retained file cannot alter child mappings. Capture
also seals in-process RAM images. Import is an eager RAM copy; child restores
remain private COW mappings, not a zero-copy workflow.
Each service result identifies the root digest and inherited decision count.
Version-5 runtime certificates distinguish this contract from fresh-boot v4.
The native validation archive retains the complete certificate directory under
`fixed-plan/`. Offline verification binds its exact embedded plan, state/RAM/
context inventory, runtime/guest artifacts, prefixes, ledgers, serial bytes,
and passing active replay. Certificate JSON alone is not a replayable root.

Boot decisions remain retained ancestry; only the resumed suffix executes
again. A restart can introduce an uncontrolled new boot. This feature does not
control instruction boundaries or kernel timers. RAM and UART ancestry can
contain secrets. Network digest ancestry is retained in host memory and the
context file; large traffic histories increase checkpoint cost. Context and VM
state are bounded to 128 MiB each, guest RAM to 64 GiB per service.

Active replay divergence during startup retains `execution-error.json` beside
the affected service's serial log, containing the first error, partial machine
trace and ledgers. It fails immediately instead of exhausting dependency rounds.

This controls concurrent emulated device effects, explicit host inputs, and
supported userspace device interrupt injection at the KVM boundary. Theseus
does not yet make guest instructions deterministic, choose Linux thread or
process execution, control when the guest services an injected interrupt, or
schedule in-kernel timer interrupts between exits.

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
  matching published arm64 kernel/module pair, or retain initialized Linux
  random state in a checkpoint. Published amd64 kernels do not include that
  arm64 seed-loader module.
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
- **Execution between KVM exits.** The machine replay gate selects and verifies
  vCPU turns, explicit host inputs, and supported userspace device interrupt
  injection at controlled boundaries. It cannot control or explain divergence
  that happens entirely between those boundaries, including guest interrupt
  servicing, in-kernel timer delivery, and guest-side input consumption.

## Replay fingerprints

Each captured timeline node records:

1. **Entropy probe** — next bytes the entropy device would serve (must be
   a fresh ChaCha stream of the node's seed).
2. **Markers** — guest log bytes through the control channel (behavioral
   coverage).
3. **Dirty pages** — KVM dirty-bitmap count at capture (memory footprint).

Replay compares these fields plus the per-vCPU exit streams and machine-wide
execution stream at every retained node. A match is evidence for those recorded
observations, not a proof about unrecorded guest state.

## Certify a runtime

On a Linux host with read/write `/dev/kvm`, run a fixed Compose plan twice and
write a support-profile witness:

```sh
theseus compose plan > plan.json
theseus-topology certify --plan plan.json --output certificate
```

The second execution is a normal locked replay of the first. Its certificate
records the plan digest, platform profile, and exact serial, entropy, storage,
network, virtual-clock, lifecycle, scheduled-action, and KVM-exit comparisons.
Certification fails closed without virtual time, KVM access, or the supported
simulated-I/O profile. It does not claim instruction-by-instruction equality
for counter reads inside a virtual-time quantum.
