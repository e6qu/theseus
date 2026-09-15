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
| Fault injection | Partial | Network, packet, partition, process, storage, and clock operations exist; asymmetric degradation, latency, clogs, CPU throttling, and a mature custom-fault interface do not. |
| Assertions and guidance | Partial | Always, sometimes, reachable, and unreachable properties exist; language-neutral bounded shell choices and a Rust helper exist, but language support and assertion-guided exploration remain narrow. |
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
- Seeded entropy, a matching kernel random-device module, simulated networking
  and storage, exit-counted virtual time, and explicit fault operations.
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
- A Test Composer-shaped lifecycle for ordinary Compose operations, including
  setup alternatives, overlapping named processes, exclusive and singleton
  drivers, anytime observations, and terminal eventual/final checks.
- Discovery of all or a named subset of executable commands under
  `/opt/antithesis/test/v1/<template>` across image services. Every timeline
  selects one template. Filename roles, bounded parallel slots, source paths,
  terminal fault recovery, and eventual process termination remain locked and
  replayable.

The baseline has important limits:

- Determinism depends on implemented interception points. Theseus does not yet
  provide instruction-level determinism for an otherwise unmodified Linux
  system.
- Guest counters can advance within an exit-counted virtual-time quantum.
- Stock-kernel `/dev/random` and `/dev/urandom` replay requires the matching
  released kernel and Theseus random-device module.
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
- The current public release includes Go module coverage and the earlier
  coverage, pthread scheduling, structured-choice, and unified-search work.
  Those capabilities remain short of product-ready until both native KVM
  certifications complete and their retained evidence is independently
  verified.

## Active delivery: autonomous fault profiles

Move common failures from hand-authored operation barriers into bounded,
replayable profiles that can accompany any discovered test template:

1. Add an opt-in default profile that derives valid network and service fault
   candidates from the locked topology instead of requiring one YAML entry per
   target.
2. Add operation-boundary service stop, kill, and restart/recovery actions with
   exact service identity and lifecycle evidence.
3. Add asymmetric latency, loss, duplication, corruption, bandwidth, queue,
   and MTU candidates for eligible directed links, using the simulated switch
   rather than host traffic control.
4. Keep startup, first commands, eventually/finally commands, and result
   collection quiet. Terminal commands must recover every active generated
   fault before running.
5. Retain the expanded candidate catalog, selected actions, recovery actions,
   and effects in plans, decision traces, replay, minimization, and reports.
6. Extend a self-contained distributed tutorial to discover multiple templates,
   explore the default profile, minimize one failure, and replay it from a
   published runtime.

Exit when a user can point Theseus at an ordinary multi-service image set,
declare a bounded profile, and obtain understandable, replayable faulted
timelines without enumerating each link and service action by hand.

## Priority 0: make the current product real for users

Close the difference between merged source, published artifacts, and retained
runtime evidence before adding another isolated runtime feature.

Deliver one coherent release-and-evidence change:

1. Fix runtime-bundle packaging so directories are handled correctly and both
   Linux architectures produce verifiable archives and SBOMs.
2. Publish the current CLI, runtime bundles, architecture images, multi-arch
   manifest, kernel, modules, instrumentation tools, build inputs, and
   attestations under the same short commit SHA.
3. Run the container, fault, coverage, schedule-search, and pthread
   synchronization tutorials on native KVM using only those published
   artifacts.
4. Retain the plans, locked workloads, complete result inventories, logs,
   reports, minimized counterexamples, replay results, host facts, and runtime
   identities.
5. Verify that a fresh released CLI can inspect, evaluate, compare, minimize,
   and replay the retained evidence without a source checkout.
6. Correct documentation and comments that describe older image or pthread
   limitations, and make claims match the released evidence.

Exit when a new user can start from the README, retrieve one released SHA, run
a representative concurrent service campaign, inspect a failure, and replay
the minimized counterexample without undocumented inputs.

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
  and environmental inputs from operation-boundary records into one
  lower-level versioned decision stream.
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
- Add asymmetric latency and loss, slow/jammed links, network clogs, process
  stop/kill/restart, CPU throttling, clock jumps, and configurable custom
  faults.
- Support quiet periods and final fault windows so startup and result
  collection are not accidentally corrupted.
- Make useful default fault profiles available while keeping every injected
  fault visible and replayable.

Exit when an ordinary distributed system can bring its existing test commands
and have Theseus autonomously compose hundreds of replayable scenarios across
parallelism, inputs, faults, and schedules.

## Priority 5: properties, events, and investigation

Turn retained evidence into an investigation workflow comparable to
Antithesis reports and multiverse debugging.

- Add `always_or_unreachable` and make property observations first-class search
  feedback.
- Provide supported assertion, event, and structured-randomness APIs for C,
  C++, Rust, Go, and Java, while retaining a language-neutral JSON event path.
- Capture stdout, stderr, structured events, faults, decisions, coverage,
  properties, and user artifacts on one ordered timeline.
- Add textual, structured, and temporal queries such as preceded-by and
  followed-by over retained event data.
- Navigate to any retained checkpoint, change one controlled choice or fault,
  re-execute, and compare alternative futures.
- Estimate and display failure probability from actual repeated experiments.
  Use causal language only when a recorded intervention supports it.
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
- Expose live progress, logs, properties, coverage, resource use, and retained
  executions while a campaign is running.
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
