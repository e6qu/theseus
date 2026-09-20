# Theseus roadmap

## Product objective

Build a product close to Antithesis in outcome and workflow: run ordinary
containerized systems in a controlled deterministic environment, explore
inputs, faults, and schedules with feedback, detect property violations, and
let a user replay and investigate a failure.

Antithesis parity is the product direction. Open-source operation,
self-hosting, arm64 support, offline artifacts, and implementation choices are
useful only when they advance that direction. They are not substitutes for a
missing Antithesis capability, and the roadmap must not prioritize novelty or
differentiation ahead of parity.

Measure progress by user-visible behavior demonstrated with released artifacts.
Implemented types, accepted configuration, generated reports, and unit tests do
not by themselves establish product capability.

Use these labels consistently:

- **Implemented:** the behavior exists in source and passes appropriate tests.
- **Demonstrated:** retained evidence shows the behavior on the claimed runtime
  and architecture.
- **Product-ready:** a user can perform the workflow from published artifacts,
  reproduce it, and understand its limitations without repository-only inputs.

## Current position against Antithesis

| Product capability | Theseus | Gap to close |
| --- | --- | --- |
| Hermetic Linux execution | Partial | Execution is controlled at selected KVM, device, operation, and instrumented application boundaries, not at whole-machine instruction and interrupt granularity. |
| Ordinary container workloads | Partial | Image-backed services and a Compose subset work; Kubernetes and broad Compose compatibility do not. |
| Deterministic replay | Partial | Seeds, locked inputs, schedules, faults, checkpoints, and bundles are retained, but uncontrolled kernel and application behavior can still escape the model. |
| Feedback-guided exploration | Partial | One bounded decision-prefix policy combines coverage, properties, topology states, structured choices, runnable sets, faults, and prior outcomes; it is not yet validated at production scale. |
| Fault injection | Partial | Explicit and topology-derived profiles cover service lifecycle, asymmetric network degradation and partitions, storage, packet, clock operations, CPU throttling, and directed link clogs, including generated throttle and clog candidates. A mature custom-fault interface remains open. |
| Assertions and guidance | Partial | Always, always-or-unreachable, sometimes, reachable, and unreachable properties exist; language-neutral bounded shell choices and a Rust helper exist, but language support and assertion-guided exploration remain narrow. |
| Coverage guidance | Partial | GCC C and Go basic blocks plus LLVM C/C++/Rust edges cover native executables, shared libraries, a selected Cargo graph, and a selected Go command's imported main-module packages. Compose locks manifests and symbols, validates them before boot, and joins source locations into reports; Rust dynamic graphs, Go external modules and CGO, Java, and production-scale validation remain open. |
| Schedule exploration | Partial | A bounded instrumented GCC C pthread path controls selected synchronization; general thread, process, futex, syscall, timer, and interrupt scheduling do not. |
| Test composition | Partial | Explicit operations and discovered Antithesis-compatible image templates use all seven lifecycle roles. Each timeline selects one template, the explorer varies bounded command concurrency, eventual checks kill live commands, and final checks join them. Production-scale adaptive command scheduling remains open. |
| Failure investigation | Early | Replay, minimization, checkpoints, reports, and history comparison exist; interactive time travel, interventions, alternative futures, temporal queries, and causal evidence do not. |
| Product operation | Early | Theseus is primarily a local/self-hosted CLI; it lacks a comparable API, CI workflow, live campaign view, scalable parallel service, notification surface, and web debugger. |

## Verified implementation baseline

Theseus currently has:

- A Linux/KVM runtime built around Firecracker.
- Container image conversion, image-backed services, topology execution, and a
  strict supported subset of Compose.
- Seeded entropy, an arm64 kernel random-device module, retained CRNG state,
  simulated networking and storage, exit-counted virtual time, and explicit
  fault operations.
- Bounded campaigns, static and adaptive case selection, checkpoint-prefix
  reuse, minimization, locked replay, comparison, evaluation, and reports.
- Serial and SDK evidence for always, sometimes, reachable, and unreachable
  properties.
- Versioned GCC C and Go block plus LLVM C/C++/Rust edge coverage. All use
  build-scoped identities in guidance, replay, comparison, evaluation, and
  reports; C and LLVM addresses are module-relative, while Go uses a
  fixed-executable program counter. Compose locks coverage manifests and symbols; the
  runner validates them before boot and joins functions and source lines into
  reports while isolating dynamically loaded native module runtimes. The CLI
  can instrument one Cargo binary and its resolved static Rust target dependencies
  as a single module while retaining package-input digests. It can also build
  one fixed-address Linux Go command with first-hit blocks across the imported
  source packages in its main module from an isolated copy.
- A bounded GCC C pthread scheduler with stable creation-order identities,
  explicit schedules, enumerated schedules, and runnable-prefix exploration.
  It controls `pthread_join`, default mutex lock/unlock, and untimed condition
  wait/signal/broadcast. It retains up to 8,192 decisions, 8,192 synchronization
  events, and 128 synchronization objects for at most 32 threads.
- Portable campaign and counterexample formats with locked input and runtime
  identities.
- Named bounded choices for image commands, point-of-use choice evidence, and
  a unified policy over operation, choice, schedule, fault, coverage,
  property, and topology-state signals.
- A replay-checked, human-readable decision trace for each Compose campaign
  run, covering operation inputs, observed choices, thread selections, and
  applied actions at deterministic operation boundaries.
- Per-vCPU rolling SHA-256 ledgers plus one bounded, exact VM-wide stream of
  handled KVM exits, emulated device effects, UART and control-channel input,
  virtual-clock jumps, and userspace device interrupt injection. Checkpoint
  children inherit the stream and undelivered UART, virtio MMIO, virtio MSI-X,
  ACPI notification, and keyboard requests. Locked replay uses it as an
  admission protocol: the next host or vCPU actor and its exact payload must
  match before the effect is admitted.
- Version-2 single-service replay plans require complete machine-stream
  evidence and install it before boot. UART events use recorded API input;
  replay retains named diagnostics and rejects missing or inconsistent evidence.
  Host-timed pauses and runtime errors are not claimed as replayable guest exits.
  Attached i8042 and system-event resets terminate on their recorded turn,
  rather than extending the stream until asynchronous event-loop shutdown.
- A Test Composer-shaped lifecycle for ordinary Compose operations, including
  setup alternatives, overlapping named processes, exclusive and singleton
  drivers, anytime observations, and terminal eventual/final checks.
- Discovery of all or a named subset of executable commands under
  `/opt/antithesis/test/v1/<template>` across image services. Every timeline
  selects one template. Filename roles, bounded parallel slots, source paths,
  terminal fault recovery, and eventual process termination remain locked and
  replayable.
- An opt-in `standard` fault profile derived from ordinary command boundaries,
  image services, and shared networks. It generates bounded service
  stop/kill/restart, directed partition, and directed packet-condition
  candidates, then recovers active faults before terminal checks.

The baseline has important limits:

- Determinism depends on implemented interception points. Theseus does not yet
  provide instruction-level determinism for an otherwise unmodified Linux
  system.
- The VM-wide execution stream controls vCPU admission at emulated-device
  boundaries and the admission of explicit UART, control-channel, and
  virtual-clock input. It injects supported userspace device interrupts on
  recorded vCPU turns. Theseus still does not control guest instruction or
  Linux thread/process ordering, when the guest services an interrupt,
  in-kernel timer delivery, or when the guest consumes queued input.
- Guest counters can advance within an exit-counted virtual-time quantum.
- Fresh-boot `/dev/random` and `/dev/urandom` seeding requires the matching
  released arm64 kernel and Theseus random-device module. Ready-checkpoint
  replay retains Linux's CRNG state instead; it does not prove repeatable boot.
- Application coverage still requires an explicit build frontend and coverage
  catalog. Rust dynamic dependencies, Go external modules and CGO, Java,
  JavaScript, .NET, and transparent instrumentation of existing images remain
  unsupported.
- Scheduling is cooperative and instrumentation-specific. Timed waits,
  cancellation, semaphores, direct futexes, blocking syscalls, `fork`/`exec`,
  uninstrumented threads, and general process scheduling can escape it or
  deadlock.
- `compare` reports differences between histories. It does not run an
  intervention and must not claim causality.
- Branch capture copies guest RAM into a memfd before children use private
  copy-on-write mappings; it is not zero-copy.
- There is no Kubernetes input, hosted campaign service, live debugger,
  temporal log query system, or broad language SDK.
- The current public release carries indexed amd64 native-KVM certification:
  the runtime certificate, the runtime-validation archive, and the retained
  distributed counterexample verify offline with `theseus evidence verify`.
  Arm64 assets are published but not yet certified.

## Released-product certification

Every successful SHA release starts amd64 native certification on a hosted
KVM runner. A passing run attaches the indexed evidence set to the release:
the fixed-plan runtime certificate, the container/coverage/schedule-search/
pthread/ordered-replay validation archive, and the retained partition,
recovery, minimized lost-update counterexample. The released CLI rejects an
incomplete, renamed, or mismatched evidence set and reports the exact
certified architecture scope.

Keep the rules that produced that evidence as standing invariants:

- The released path executes the exact digest-pinned published binaries; it
  never substitutes source CI or packaging success for native execution.
- The source and released paths invoke one counterexample driver whose
  recovery event is `recovery_probe_sent`.
- `compare` stays observational; replay admission is decided by the declared
  control projection and explicit replay checks.
- Released-path containers hand generated outputs back to the invoking user,
  and failed certification retains partial guest evidence and serial logs.

Remaining in this track: certify arm64 on a labelled self-hosted KVM runner
when one is online. Never substitute emulation or imply that packaging
proves native execution. With the amd64 gate closed, the next work is
Priority 1.

Source CI qualifies the same portfolio against the source runtime built in
release mode: UART/reset replay, standard random devices from retained CRNG
state, whole-topology roots, ordinary containers, C coverage, thread-schedule
search, pthread synchronization, ordered KVM-exit replay, and the distributed
counterexample. Source runs record the commit and actual binary hashes and
must not issue a publishable release index.

`theseus compose verify` checks portable ready-checkpoint bundles offline:
locked inputs, configuration identity, state/RAM/context, ancestry, UART
logs, and full machine/vCPU execution ledgers. It does not deserialize native
CPU state or certify execution provenance. Retained RAM may contain secrets.
A 512 MiB LRU budget bounds cached checkpoint-prefix RAM, excluding the
immutable root and active working branches; it is not a process-RSS limit.

## Priority 1: deterministic execution and scheduling plane

This is the largest technical gap to Antithesis and takes precedence over
adding more narrow wrappers.

Build a single ordered execution-decision stream that can control and replay:

- Linux threads and processes across `clone`, `fork`, `exec`, and exit.
- Futex waits and wakes, blocking syscalls, signals, timers, and readiness.
- Interrupt and device-input delivery.
- Virtual clock reads and advancement without mid-quantum leakage.
- Random and other external inputs consumed by the guest.
- Network, storage, and process faults at exact replayable positions.

Completed slices serialize handled exits and emulated device effects into one
branch-aware machine stream while retaining each vCPU's local stream, then add
explicit UART input, control-channel input, and virtual-clock jumps to that
same protocol. Deterministic runs now also observe in-kernel timer deliveries —
the x86 LAPIC timer and the aarch64 arch timer fire inside the irqchip without
a KVM exit — and retain them as observation evidence beside each execution
capture. An opt-in amd64 hold mode goes further: it clears asserted
LAPIC-timer deliveries and injects them at recorded vCPU turns that exact
replay gates, turning host-drift episode movement into a divergence instead of
a silent pass. Arming still depends on guest counter reads, so held deliveries
are not yet fully deterministic. Deterministic VMs no longer hand supported userspace device
interrupts to asynchronous irqfds: they retain level and edge requests across
checkpoints, wake a running vCPU, and inject each UART, virtio MMIO, virtio
MSI-X, ACPI notification, or keyboard request through KVM as an exact recorded
vCPU turn. CTRL+ALT+DEL is also an exact host-input decision. Replay gates the
next vCPU or host actor and rejects a changed payload, interrupt, checkpoint
prefix, missing suffix, or extra effect. Writes and read identities are gated
before device access; read values are necessarily checked afterward. Guest
reset is a terminal admission turn, not an asynchronous host-timed cutoff.

Fixed-run replay can enforce that complete exit stream. Portable
checkpoint-backed campaigns currently enforce its explicit host-input
projection while retaining all intervening exits and userspace interrupt
deliveries as evidence. Exported counterexamples preserve their global
cross-service input order and finish at their last operation checkpoint;
acceptance uses declared properties and controlled topology effects rather
than unstable whole-execution fingerprints. They do not claim identical Linux
execution or interrupt timing between controlled turns; making the full stream
portable requires the runnable-entity and instruction-boundary control in the
next slice.

The next slice must control in-kernel timer delivery, then runnable guest
entities, virtual-clock reads, and guest-side input consumption between KVM
exits. Controlled injection is not general interrupt determinism: the guest
can still service an injected interrupt at an uncontrolled instruction
boundary.

Move control into the lowest practical kernel, hypervisor, or paravirtualized
boundary. Application instrumentation may expose semantics and coverage, but
correctness must not depend on wrapping every synchronization API used by a
service.

Retain stable process/thread identities, runnable sets, chosen actions,
external inputs, and checkpoint ancestry. Reject replay when the observed
decision stream diverges instead of silently continuing.

Exit when an otherwise ordinary multithreaded and multiprocess Linux service
can exhibit a timing-dependent failure, be minimized, and replay from retained
artifacts without depending on host timing.

## Priority 2: unified feedback-guided exploration

Extend the bounded unified decision-prefix engine into a general decision-tree
search system modeled on the workflow Antithesis exposes.

- Move structured choices, runnable selections, fault choices, test actions,
  and remaining environmental inputs from operation-boundary records into the
  lower-level versioned decision stream. Explicit UART, control-channel, and
  virtual-clock inputs already use the machine stream.
- Reuse checkpoints at common prefixes and explore alternative suffixes.
- Combine coverage novelty, property progress, rare states, fault outcomes,
  schedule outcomes, and execution cost in the search policy.
- Add structured choice APIs with immediate-use semantics so the engine can
  learn which generated values matter.
- Make every discovered execution replayable from an exact decision prefix;
  keep a human-readable explanation alongside the machine record.
- Evaluate changes on fixed-budget public workloads, including comparisons
  with unguided seeds and each individual feedback signal.

Exit when the unified engine finds useful states or failures more reliably
than seed enumeration on published benchmarks and every reported improvement
is independently reproducible.

## Priority 3: broad coverage and semantic instrumentation

Coverage must work on realistic services rather than only tutorial C binaries.

The source tree has GCC C and Go blocks plus LLVM C/C++/Rust edges with stable
module/build identities. Native frontends retain module-relative addresses
through PIE, ASLR, and dynamic loading; Go uses fixed-address executables. The
CLI can build selected Cargo and Go dependency graphs. Compose locks declared
manifests and symbols, verifies them before boot, and joins them to report
source locations even when deployed binaries are stripped. The next work is:

1. Validate large multi-service symbol catalogs on both released Linux
   architectures and retain native-KVM evidence for stripped executables and
   multiple DSOs.
2. Extend Go coverage to external module graphs and CGO, add a representative
   Java path, then select JavaScript and .NET work from real workload demand.
3. Compare block, edge, sampled-PC, and unguided search under the same public
   campaign budgets; retain every workload and result.

Exit when multi-service workloads built with supported production toolchains
produce stable, source-associated coverage that guides exploration and
survives replay, minimization, and artifact export.

## Priority 4: complete test templates and the fault model

Make it possible to express the same testing workflow users expect from
Antithesis without constructing low-level campaign schedules by hand.

- Let the explorer vary command ordering, parallelism, structured inputs,
  faults, and schedules while keeping lifecycle contracts intact.
- CPU throttling, directed link clogs, and guest-clock rate windows are
  explicit campaign faults and `standard`-profile candidates now, and clock
  jumps move backward as well as forward. Remaining in this slice:
  configurable custom faults in the generated profile model.
- Generalize quiet periods and explicit fault windows beyond the current
  lifecycle roles and automatic terminal recovery.

Exit when an ordinary distributed system can bring its existing test commands
and have Theseus autonomously compose hundreds of replayable scenarios across
parallelism, inputs, faults, and schedules.

## Priority 5: properties, events, and investigation

Turn retained evidence into an investigation workflow comparable to
Antithesis reports and multiverse debugging.

- `always_or_unreachable` exists as a campaign property kind. Make property
  observations first-class search feedback.
- Provide supported assertion, event, and structured-randomness APIs for C,
  C++, Rust, Go, and Java, while retaining a language-neutral JSON event path.
- Capture stdout, stderr, structured events, faults, decisions, coverage,
  properties, and user artifacts on one ordered timeline.
- Add textual, structured, and temporal queries such as preceded-by and
  followed-by over retained event data.
- Navigate to any retained checkpoint, change one controlled choice or fault,
  re-execute, and compare alternative futures.
- Campaign reports display each future's observed failure frequency from the
  retained timelines. Causal language still requires a recorded intervention.
- Allow users to collect artifacts immediately before and after a selected
  property violation or event.

Exit when a user can move from a failed property to its relevant logs and
decisions, fork an earlier state, test an alternative, and share the complete
reproducible investigation.

## Priority 6: product surface and workload compatibility

Once the execution, exploration, and investigation loops work end to end,
make them available as a complete product rather than a collection of CLI
commands.

- Add a stable campaign API and CI integration alongside the CLI.
- Campaign exploration streams one structured progress line per completed
  timeline on stderr. Remaining live surface: logs, coverage, resource use,
  and retained executions while running.
- Run many deterministic workers in parallel with explicit resource budgets
  and reproducible work allocation.
- Add notifications and machine-readable result retrieval.
- Accept standard Kubernetes manifests and Helm inputs through a documented
  supported environment, in addition to expanding Compose compatibility.
- Add a web investigation interface only on top of the same portable evidence
  and APIs used by the CLI.

Exit when a team can launch, observe, gate CI on, and investigate Theseus
campaigns through documented CLI and API workflows without operating internal
repository machinery.

## Delivery rules

Apply these rules to every priority:

1. Prefer a coherent vertical product slice over an anemic syntax-only or
   plumbing-only PR. Keep commits separately reviewable inside that PR.
2. Do not pursue differentiation-first work while a higher-priority Antithesis
   capability remains absent.
3. Label behavior as implemented, demonstrated, or product-ready. A hash proves
   retained bytes, not execution.
4. For KVM evidence, record the architecture, host kernel, KVM API,
   kernel/module pair, runtime digest, plan, decision stream, and I/O profile.
5. Preserve replay plans, locked workloads, results, logs, fault schedules,
   property verdicts, checkpoints, and artifact inventories for public
   counterexamples.
6. Evaluate exploration and performance changes against fixed-budget public
   workloads, not only unit tests or synthetic counters.
7. Keep only one implementation PR open at a time unless the project explicitly
   changes that policy.
8. Do not restore historical completion ledgers. Git history records completed
   work; this file describes the current truth and next work.

## Documentation and tutorial contract

Every behavioral PR must update affected tutorials, references, comments, CLI
wording, comparisons, and this roadmap in the same change.

Tutorials must:

- Be directly reachable from the root README.
- Use imperative, concise, step-by-step prose with expected observations.
- Treat the tutorial directory as both the working directory and the complete
  context.
- Depend on Theseus only through already published binaries or container
  images.
- Include every workload, configuration, and script they need.
- Avoid wrapper scripts when the commands can be shown directly. When a script
  is the workload or a necessary low-level tool, show its purpose and keep it
  reviewable before execution.
- Never rely on hidden checkout-relative files, historical random-device
  examples, unpublished artifacts, or unsupported product claims.

CI should detect stale capability language, hidden tutorial dependencies,
unreviewable wrappers, and claims that exceed released evidence.

## Antithesis capability references

Use the current Antithesis documentation as the parity reference for:

- [Deterministic simulation testing](https://antithesis.com/docs/resources/deterministic_simulation_testing/)
- [Test templates](https://antithesis.com/docs/product/writing_tests/test_templates/)
- [Test Composer lifecycle](https://antithesis.com/docs/product/writing_tests/test_templates/test_composer_reference/)
- [Coverage instrumentation](https://antithesis.com/docs/product/writing_tests/instrumentation/coverage_instrumentation/)
- [Fault types](https://antithesis.com/docs/product/writing_tests/controlling_faults/fault_types/)
- [Assertions](https://antithesis.com/docs/product/writing_tests/assertions/)
- [Structured randomness](https://antithesis.com/docs/reference/sdk/generate_randomness/)
- [Debugging and causality analysis](https://antithesis.com/docs/product/debugging/)
- [Event logs and temporal queries](https://antithesis.com/docs/reference/event_logs/)
- [Launching tests](https://antithesis.com/docs/product/launching_tests/)
- [Kubernetes setup](https://antithesis.com/docs/setup/kubernetes/)

These references define target user capabilities. They are not evidence that
Theseus currently implements them.
