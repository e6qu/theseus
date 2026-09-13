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
| Search guidance | Coverage-guided autonomous exploration | Markers, topology/property evidence, dirty-page footprint, and sampled guest PCs |
| Coverage | Application basic-block instrumentation | Raw guest-PC signals; no application basic-block instrumentation yet |
| Faults | Network, process, clock, and storage faults | Simulated network/storage plus Compose lifecycle, clock, and packet actions |
| Concurrency | Controlled thread/process scheduling | Operation-level overlap only; no general thread scheduler control yet |
| Debugging | Time-travel and causality analysis | Static reports, replay, minimization, bundle comparison, snapshot export |
| Causality | Counterfactual re-exploration from checkpoints | Not implemented; comparison only finds recorded differences |
| Delivery | Hosted commercial product | Open source, self-operated Linux/KVM runtime |

## What Antithesis demonstrates that Theseus does not yet

### Application coverage

Antithesis documents compiler-based basic-block instrumentation for coverage.
Theseus currently collects guest instruction addresses by single-stepping
small guests or sampling vCPU PCs at deterministic exits and pause barriers.
Those addresses can guide search, but they are not application basic-block or
edge coverage.

Reference: [Antithesis coverage instrumentation](https://antithesis.com/docs/product/writing_tests/instrumentation/coverage_instrumentation/).

### General concurrency exploration

Antithesis controls execution deeply enough to pause threads and explore
scheduling choices. Theseus can overlap declared operations and control
topology faults, but it does not yet expose a general deterministic scheduler
for application threads or processes.

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
