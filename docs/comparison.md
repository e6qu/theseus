# Theseus compared with Antithesis

Antithesis is the closest product reference for Theseus: both run distributed
systems under controlled faults and retain reproducible failures. They do not
currently provide equivalent behavior.

This comparison describes public interfaces, not benchmark results. Theseus
has no independent head-to-head evaluation with Antithesis.

| Capability | Antithesis | Theseus today |
|---|---|---|
| Workload packaging | Container-based test environment | Container images or explicit Firecracker guest inputs |
| Test interface | Test templates and SDK assertions | Compose campaigns, UART operations, serial properties, optional SDK |
| Determinism | Custom deterministic hypervisor | KVM plus seeded devices, simulated I/O, and exit-counted virtual time |
| Replay | Instruction-level deterministic reproduction | Locked-input replay with recorded fingerprints; mid-quantum clock caveat |
| Search guidance | Coverage-guided autonomous exploration | One bounded decision-prefix policy combines structured choices, C application blocks, runnable selections, faults, properties, topology states, and sampled guest PCs |
| Coverage | Application basic-block instrumentation across documented language toolchains, symbol ingestion, and report validation | GCC C blocks plus LLVM C/C++/Rust edges, locked symbol catalogs, and automatic source joins for PIE and dynamically loaded native modules; no Go/Java instrumentation yet |
| Faults | Network, process, clock, and storage faults | Simulated network/storage plus Compose lifecycle, clock, and packet actions |
| Concurrency | Controlled thread/process scheduling | Operation overlap plus bounded GCC C pthread basic-block scheduling, runnable-set feedback, default mutexes, and untimed condition variables; no general Linux scheduler control |
| Structured choices | SDK choices consumed at the point of use | Language-neutral bounded shell-operation choices and a Rust SDK helper, retained and replay-checked at the operation boundary |
| Debugging | Time-travel and causality analysis | Static reports, replay, minimization, bundle comparison, snapshot export |
| Causality | Counterfactual re-exploration from checkpoints | Not implemented; comparison only finds recorded differences |
| Delivery | Hosted commercial product | Open source, self-operated Linux/KVM runtime |

## What Antithesis demonstrates that Theseus does not yet

### Application coverage

Antithesis documents compiler-based basic-block instrumentation across C/C++,
Rust, Go, Java, JavaScript, .NET, and other LLVM toolchains. Theseus packages a
GCC C basic-block frontend and LLVM edge frontends for C, C++, and standalone
Rust binaries and one Cargo binary's static Rust target dependencies. The LLVM
runtime preserves independent build-scoped identities
for PIE executables and dynamically loaded native libraries; its inspection
tool validates sanitizer guards and resolves retained module offsets with the
matching symbols. Compose coverage catalogs lock manifests and symbols into
replay bundles, revalidate them before boot, and attach functions and source
lines to campaign reports. Campaign guidance, replay, comparison, evaluation,
and reports retain the coverage records. Theseus does not yet support Rust
dynamic dependency graphs or the non-LLVM language set. A controlled public
search comparison remains pending.

Reference: [Antithesis coverage instrumentation](https://antithesis.com/docs/product/writing_tests/instrumentation/coverage_instrumentation/).

### General concurrency exploration

Antithesis controls execution deeply enough to pause threads and explore
scheduling choices across supported workloads. Theseus now has a narrower
source implementation: its packaged GCC C frontend serializes instrumented
pthread basic blocks from an explicit repeating schedule, up to 256
pre-enumerated periodic patterns, or a bounded tree of prefixes derived from
runnable sets observed during execution. It assigns identities in creation
order and retains prefixes, runnable sets, and selected threads for replay.
It is limited to 32 threads and 8,192 decisions. It controls joins, default
mutex locking, and untimed condition waits/signals/broadcasts, but not timed
waits, cancellation, semaphores, direct futex use, blocking I/O, or
uninstrumented libraries. It has no process scheduler and still needs native KVM
evidence. It is not equivalent to Antithesis's general concurrency control.

### Unified choices and search

Theseus can now lock named bounded values, require a point-of-use choice
record, and search operation inputs, choice assignments, runnable prefixes,
and faults through one decision-prefix policy. Coverage, property witnesses,
topology novelty, choice novelty, and scheduling novelty contribute to that
policy, and replay checks a canonical decision trace. The remaining gap is
substantial: choices are currently finite Compose shell inputs, the policy has
not been validated on published fixed-budget benchmarks, and lower-level Linux
execution events can still escape control.

### Test templates

Antithesis provides setup, workload, and teardown templates for unmodified
containers. Theseus has analogous campaign concepts, but its current interface
is lower-level: Compose configuration, a designated driver, explicit UART or
image operations, and serial evidence.

Reference: [Antithesis test templates](https://antithesis.com/docs/product/writing_tests/test_templates/).

### Counterfactual causality analysis

Antithesis describes re-executing from checkpoints while changing one event to
test whether it caused a later behavior. Theseus `compare` reads two completed
histories and reports their first retained difference. It does not run those
counterfactual experiments and its output must not be called a causal result.

Reference: [Antithesis causality analysis](https://antithesis.com/docs/product/debugging/causality_analysis/).

### Scale and operating model

Antithesis is a hosted product with a mature autonomous exploration service.
Theseus is a self-operated development project. It needs published runtime
artifacts, native KVM capacity, retained certificates, and reproducible public
workload evidence before performance or bug-finding comparisons would be
meaningful.

## Where Theseus is intentionally different

- The Firecracker fork, engine, CLI, and bundle formats are inspectable and
  modifiable under open-source licenses.
- KVM keeps the runtime close to commodity Linux virtualization, at the cost
  of weaker instruction-level control.
- The artifact contract is explicit: a bundle locks the selected runtime,
  workload, plan, and recorded evidence for offline inspection.
- The CLI can validate, plan, report, evaluate, and compare bundles without a
  hosted service. Execution still requires Linux and KVM.

## Other useful comparisons

- Hypothesis, QuickCheck, and proptest explore inputs inside one process; they
  do not simulate a distributed runtime.
- FoundationDB and TigerBeetle obtain stronger determinism by building the
  application against a deterministic simulator. Theseus instead targets
  ordinary Linux binaries across a VM boundary.
- Jepsen analyzes consistency histories from real deployments; it does not
  provide deterministic VM replay.
- Chaos tools inject faults into live infrastructure but generally do not
  control all execution inputs or produce locked deterministic replays.

These tools are complementary. Choose based on the system boundary and the
evidence required, not by treating the word “deterministic” as one uniform
guarantee.
