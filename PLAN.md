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
The current gap-by-gap position against Antithesis, with a prioritized fix
list, is maintained in
[the parity analysis](docs/audits/2026-09-antithesis-parity.md).
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
| Ordinary container workloads | Partial | Image-backed services, a Compose subset, and a Kubernetes subset (Pods, Deployments, StatefulSets, DaemonSets, ReplicaSets, Jobs, multi-container pods, ConfigMap/Secret volumes, manifest directories, rendered Helm charts) work; Compose breadth beyond the audited subset does not. |
| Deterministic replay | Partial | Seeds, locked inputs, schedules, faults, checkpoints, and bundles are retained, but uncontrolled kernel and application behavior can still escape the model. |
| Feedback-guided exploration | Partial | One bounded decision-prefix policy combines coverage, properties, topology states, structured choices, runnable sets, faults, and prior outcomes; it is not yet validated at production scale. |
| Fault injection | Partial | Explicit and topology-derived profiles cover service lifecycle, asymmetric network degradation and partitions, storage, packet, clock operations including backward jumps and rate windows, CPU throttling, directed link clogs, and user-declared custom commands inside image-backed services, including generated candidates: the standard profile also proposes custom candidates that re-run a service's own declared commands at eligible barriers. |
| Assertions and guidance | Partial | All five quantifiers, automatic crash/completion/OOM verdicts, cross-run catalogs with a suite diff (added, retired, flipped), and bounded structured choices with seeded novelty exist; assertion-guided exploration remains narrow, and JavaScript/.NET SDKs are demand-gated. |
| Coverage guidance | Partial | GCC C and Go basic blocks plus LLVM C/C++/Rust edges cover native executables, shared libraries, a selected Cargo graph, and a selected Go command's imported main-module packages; a Java agent records class-load coverage for one selected JAR with build-scoped identities and locked class-to-offset symbol maps. Compose locks manifests and symbols, validates them before boot, and joins source locations into reports; Rust dynamic graphs, Go external modules and CGO, Java method-level probes, JavaScript, and .NET remain open. |
| Schedule exploration | Partial | A bounded instrumented GCC C pthread path controls selected synchronization; general thread, process, futex, syscall, timer, and interrupt scheduling do not. |
| Test composition | Partial | Explicit operations and discovered Antithesis-compatible image templates use all seven lifecycle roles. Each timeline selects one template, the explorer varies bounded command concurrency, eventual checks kill live commands, and final checks join them. Production-scale adaptive command scheduling remains open. |
| Failure investigation | Substantial | Replay, minimization, checkpoints, reports, moment-scoped queries through one shared predicate grammar with windowed, occurrence-addressed, and bracketing relations, counterfactual forks over faults, choices, and moment slices, digest-verifiable answer exports that promote into property declarations, and a cross-set assertion diff exist; interactive time travel, executions that start at a checkpoint, and causal evidence do not. |
| Product operation | Early | The CLI, read-only HTTP evidence surface, live journals, campaign API record, and a documented CI gate workflow exist; a hosted campaign service and web debugger do not. |

## Verified implementation baseline

Theseus currently has:

Runtime and determinism:

- A Linux/KVM runtime built around Firecracker: seeded entropy (an arm64
  random-device module, retained CRNG state at ready checkpoints), simulated
  networking and storage, exit-counted virtual time in milli-unit vclock
  arithmetic (2-16x speedups and 0.1-0.9 slowdowns), and explicit fault
  operations.
- One branch-aware machine stream per execution - per-vCPU SHA-256 ledgers
  plus a bounded, exact VM-wide record of handled KVM exits, emulated device
  effects, UART/control-channel/virtual-clock input, and userspace device
  interrupts (UART, virtio MMIO/MSI-X, ACPI, keyboard) injected as recorded
  vCPU turns. Locked replay admits the next actor and payload by name;
  observed in-kernel timer deliveries are evidence, and an opt-in amd64 hold
  mode injects them at recorded turns.
- Version-2 replay plans require complete machine-stream evidence;
  checkpoint-backed campaigns gate their explicit host-input projection and
  retain the rest as evidence. Ready-checkpoint roots retain whole-topology
  state, including simulated NIC/switch queues. `compose verify` checks
  portable bundles offline; branch capture is memfd + copy-on-write, not
  zero-copy.

Coverage and scheduling:

- GCC C and Go block coverage plus LLVM C/C++/Rust edge coverage, a Java
  class-load agent for a selected JAR, and CLI frontends that instrument one
  Cargo binary with its static Rust dependencies and one fixed-address Go
  command - all with build-scoped identities locked into plans, validated
  before boot, and joined into reports. Rust dynamic graphs, Go external
  modules and CGO, Java method-level probes, JavaScript, and .NET remain
  unsupported.
- A bounded GCC C pthread scheduler: stable creation-order identities,
  explicit and enumerated schedules, runnable-prefix exploration, control of
  joins, default mutexes, and untimed condition waits - up to 32 threads,
  8,192 decisions, and 128 synchronization objects.

Workloads, faults, and campaigns:

- Image-backed services (converted from ordinary images, rootfs guarded
  against guest memory) and a strict, audited Compose subset; a Kubernetes
  subset (Pods, Deployments, StatefulSets, DaemonSets, ReplicaSets, Jobs,
  multi-container pods, ConfigMap/Secret volumes, manifest directories so
  rendered Helm charts work as-is) translating into the same locked plan.
- The Test Composer-shaped operation lifecycle (seven roles, one template per
  timeline, discovered under `/opt/antithesis/test/v1`) with invalid
  combinations removed from the corpus, plus bounded named choices for image
  commands with point-of-use choice evidence and grammar-locked bounds.
- The `standard` fault profile (service stop/kill/restart, directed
  partitions, packet conditions, custom candidates re-running a service's own
  commands), user-declared `custom` barrier faults, quiet periods and fault
  windows, CPU throttling, directed link clogs, and clock faults (backward
  jumps, rate windows).
- Bounded campaigns with static and adaptive selection, checkpoint-prefix
  reuse, minimization, locked replay, comparison, evaluation, reports,
  `--shard INDEX/TOTAL` worker partitions, and completion notification hooks.
  Every run carries a replay-checked decision trace; default
  crash/completed/OOM verdicts classify kernel OOM evidence; `status` answers
  "what did this campaign conclude" in one stable shape.
- Deterministic timer arming, stages 1-2 and the pairing: on amd64,
  `filter_timer_msrs` steers the guest into x2APIC mode and routes the
  timer registers (TMICT/TMCCT/TDCR and the TSC-deadline MSR) to
  userspace; in hold mode every arming write is a recorded
  machine-stream decision admitted by replay, and the held delivery
  admits only against its own arm (the immediately preceding stream
  decision). The COM2 debug serial console (port 0x2f8, IRQ 3,
  output-only, `debug_serial_out_path`) carries kernel diagnostics on a
  channel separate from the workload's data port. The halt frontier
  (making the delivery position virtual) is in What remains. The
  userspace APIC mode (`userspace_apic`) is landed and its workload is
  demonstrated: an emulated local APIC and IOAPIC replace the in-kernel
  irqchip, both arming paths (the x2APIC MSRs and the xAPIC MMIO page)
  schedule generation-gated host-paced wakes, delivery rides the KVM
  events path, and the CI probe boots the guest through /init, runs the
  serial sensor handshake, and reads the measurement back.

Investigation:

- A moment-addressed query surface: every boundary carries
  `<vtime_ns>@<input_sha256>`, retrievable with navigation, enumeration,
  digest-auditable collect bundles, and temporal relations - needles,
  event-field predicates, `--where` compositions, `--within` windows,
  `--anchor` occurrence addressing, and `--surrounded-by` brackets - all
  through one predicate grammar (RFC 9535 JSONPath, `where` conditions,
  array quantifiers, nested `all`/`any`/`none`) shared verbatim with the
  property layer, which also evaluates `sequence_windows`: ordered,
  byte-bounded steps over complete transcripts. The same answers are served
  over HTTP and asserted by the smoke script.
- Counterfactual forks of retained runs - fault substitutions
  (`--replace-fault`), choice substitutions (`--set-choice`), and
  moment-addressed slices (`--at-moment`) - re-executed from the shared
  deterministic prefix and diffed by `compare --forked` at the first
  diverging boundary with both sides' moment addresses. Exports carry the
  answer beside a digest manifest and promote its predicate into a standing
  property declaration. `history` aggregates properties, assertions,
  choices, and events across campaigns, and diffs assertion identities
  between campaign sets (added, retired, flipped).

Delivery:

- The read-only HTTP surface (`theseus serve`): registry, route manifest,
  per-bundle evidence routes, rendered reports, live progress journals, query
  and history routes, the campaign API record, and the comparison route -
  GET-only with Range following, locked by a CI smoke script and a documented
  gate workflow. Guest SDKs for Go, Java, C/C++, and Rust share one
  byte-identical protocol (markers, assertions, checkpoints, bounded
  choices, ordered JSON event export).
- The weighting evidence loop: `scripts/compare_weighting_signals.sh` runs
  seeded/unseeded arms for three bounded-choice workloads per budget (3 and
  6) in CI's native-KVM job, tabulating ordering, yields, and the
  seed-vs-corpus yield delta per run. The recordings: budget 6 saturates,
  budget 3 keeps the breadth-first prefix (novelty acts only through
  accumulated observations), and - after fixing a silent seed drop beside
  `--expect-counterexample` - all three workloads reproduce the fresh-first
  reordering with real seeds. The seed-default decision reads the asserted
  yield table and stays open until the delta favors the seed or the arms
  tie at every budget.

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
- There is no hosted campaign service or live debugger. Kubernetes and
  Helm inputs, the moment-addressed temporal query system, and SDKs for
  Go, Java, C, C++, and Rust have landed; JavaScript and .NET SDKs remain
  demand-driven.
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

Landed so far: handled exits and emulated device effects serialize into one
branch-aware machine stream with per-vCPU local streams; UART, control-channel,
and virtual-clock inputs join the same protocol; userspace device interrupts
and CTRL+ALT+DEL inject as exact recorded vCPU turns; in-kernel timer
deliveries are observed, and an opt-in amd64 hold mode injects them at
recorded turns (arming still depends on guest counter reads). Replay gates the
next actor and payload; guest reset is a terminal admission turn. Fixed-run
replay enforces the complete stream; portable checkpoint-backed campaigns
enforce its explicit host-input projection.

The userspace APIC is landed and its workload is demonstrated end to end
in CI (the emulated local APIC and IOAPIC, both arming paths scheduled,
delivery through the KVM events path; the full arc and the remaining
slivers - replay-exact delivery positions and the accelerated stored
deadlines - are recorded in the timer-arming design doc). The next
slices, in order: runnable guest entities, virtual-clock reads, and
guest-side input consumption
between KVM exits. Controlled injection is not general interrupt determinism:
the guest can still service an injected interrupt at an uncontrolled
instruction boundary.

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
  schedule outcomes, and execution cost in the search policy, with retained
  side-by-side comparisons on the public workloads as the evidence.
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
2. Extend Go coverage to external module graphs and CGO, deepen Java beyond
   class-load coverage to method-level probes, then select JavaScript and
   .NET work from real workload demand.
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
- Grow the explicit fault set - CPU throttling, directed link clogs,
  guest-clock rate windows, backward clock jumps, and user-declared
  `custom` barrier faults - while keeping every fault recoverable,
  replay-checked, and composable with the others.
- Extend lifecycle-aware fault windows (`until`, `quiet`) beyond the
  terminal recovery path as the fault model grows.

Exit when an ordinary distributed system can bring its existing test commands
and have Theseus autonomously compose hundreds of replayable scenarios across
parallelism, inputs, faults, and schedules.

## Priority 5: properties, events, and investigation

Turn retained evidence into an investigation workflow comparable to
Antithesis reports and multiverse debugging.

- Make property observations first-class search feedback.
- Provide supported assertion, event, and structured-randomness APIs for C,
  C++, Rust, Go, and Java, while retaining a language-neutral JSON event
  path.
- Capture stdout, stderr, structured events, faults, decisions, coverage,
  properties, and user artifacts on one ordered timeline.
- Add richer temporal operators over complete transcripts beyond the
  moment-space needle and event-field relations.
- Navigate to any retained checkpoint, change one controlled choice or
  input, re-execute, and compare alternative futures beyond the
  fault-decision forks.
- Report causal language only with a recorded intervention; render each
  future's observed failure frequency from the retained timelines.
- Allow users to collect artifacts immediately before and after a selected
  property violation or event.

Exit when a user can move from a failed property to its relevant logs and
decisions, fork an earlier state, test an alternative, and share the complete
reproducible investigation. Landed: retrieval, navigation, the shared
predicate grammar, the temporal relations and their operators, the forks,
exports and promotion, the catalogs and their diff, the live journals, and
the serve surface - all described in the verified baseline. Remaining:
property observations as first-class search feedback, and the demand-gated
SDK breadth.

## Priority 6: product surface and workload compatibility

The investigation loops work end to end; the remaining product work is
availability - run it as a service, not only as a collection of CLI
commands.

- Grow the stable campaign API record and its CI integration into a
  hosted campaign service alongside the CLI.
- Campaign exploration streams one structured progress line per completed
  timeline on stderr, and `--notify COMMAND` runs a completion hook after
  retention. Remaining live surface: logs, coverage, resource use, and
  retained executions while running.
- Run many deterministic workers in parallel with explicit resource budgets
  and reproducible work allocation; `--shard INDEX/TOTAL` partitions the
  candidate corpus deterministically across workers.
- Machine-readable result retrieval exists per surface (report, query,
  compare, evaluate); notifications compose on those evidence formats.
- Accept standard Kubernetes manifests and Helm inputs through a documented
  supported environment, in addition to expanding Compose compatibility.
- Add a web investigation interface only on top of the same portable evidence
  and APIs used by the CLI.

Exit when a team can launch, observe, gate CI on, and investigate Theseus
campaigns through documented CLI and API workflows without operating internal
repository machinery.

## What remains

Three fronts finish the roadmap. Each is blocked on something real - a
measurement, a KVM host, or a product decision - not on design.

1. **Decide the seeding default from yield.** The instrumentation is
   done: every CI run records three seeded/unseeded workload pairs and
   asserts a yield table (witnesses per run, first witness position);
   `scripts/weighting_history.py` tabulates the accumulation in one
   command (`--json` for machine reads). Today the arms tie at every
   workload and budget. The decision resolves itself when accumulated
   runs show a yield delta favoring the seed - or provable ties at
   every budget, which retires the question.
2. **The execution plane (Priority 1).** The arming arc is settled:
   the filter, the arm records, and the delivery pairing are landed,
   the halt-time delivery probe settled its own frontier with the
   kernel's own account (`LOC: 0` - the in-kernel irqchip never
   surfaces a userspace-asserted timer vector, so hold-mode timer
   determinism requires the userspace APIC), and the userspace APIC
   itself is now landed with its workload demonstrated end to end in
   CI
   ([deterministic-timer-arming.md](docs/design/deterministic-timer-arming.md)
   records the full arc: the mechanism, the probed attempts with
   their evidence, the CPUID/MSR/TSC-frequency exposure the guest
   needs to see the APIC at all, and the option analysis). The
   kernel-visible diagnostic loop (kernel log and interrupt account
   on the COM2 debug console, retained per run) is standing
   infrastructure for the next KVM-host slice. Then runnable guest
   entities and virtual-clock reads. The forks' remaining sliver
   (executing from the nearest checkpoint) is analyzed in
   [checkpoint-addressed-forks.md](docs/design/checkpoint-addressed-forks.md).
3. **Product operations (Priority 6).** A hosted campaign service on
   the stable API record, catalog management UX, and a web
   investigation interface over the same portable evidence - each a
   product decision plus engineering, with the evidence formats and
   gate workflow already in place.

Demand-gated breadth (JavaScript/.NET SDKs, Java method probes, Go
external modules and CGO, Rust dynamic graphs, Compose breadth) waits
for real workloads asking for it, per the audit. When any Priority 1-3
item lands, re-run the parity analysis per its review trigger.

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
