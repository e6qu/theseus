# Runtime and documentation claim audit — 2026-09

This audit reviewed the Theseus-authored root and component READMEs, active
design documents, tutorials, evaluation notes, workflows, and behavioral code
comments. Vendored Firecracker history and general upstream documentation were
left unchanged unless a Theseus instruction depended on them.

The dispositions below distinguish implementation from demonstrated runtime
behavior. “Implemented” means a code path and tests exist. “Demonstrated” needs
retained output from the named execution. A workflow definition is not itself
a demonstration.

| Claim area | Disposition | Active wording or evidence |
|---|---|---|
| Published platforms | Implemented packaging | Linux amd64/arm64 runtimes and CLI binaries plus macOS arm64 CLI are the intended release matrix. KVM execution remains Linux-only. |
| Image pivot provenance | Implemented packaging | Each Linux build compiles the static pivot for its explicit architecture before the image adapter. The runtime carries the pivot bytes and a source-commit/digest manifest; `theseus-image pivot` reports the embedded copy. |
| Native runtime support | Conditional | The certification workflow resolves the full commit from signed SHA-release inputs and requests self-hosted amd64/arm64 KVM jobs. It publishes only a complete indexed pair after the released CLI verifies both certificates, archive inventories, runtime digests, recovery path, and counterexample. A definition or partial run is not runtime evidence. |
| Linux random devices | Implemented with guest cooperation | Seeded virtio entropy is insufficient for a stock Linux CSPRNG. Tutorials 1–2 use the matching published kernel/module pair and only `/dev/random` and `/dev/urandom`. |
| Virtual time | Implemented with a known leak | Time advances at exit-counted boundaries; counter reads within a quantum can reflect host progression. No instruction-exact claim remains. |
| Network and storage faults | Implemented | Deterministic mode uses simulated network and memory-backed storage. Fault candidates are optional unless marked required; required actions survive minimization. Host-backed nondeterministic paths are rejected by the supported profile. Runtime proof is per retained certificate/campaign. |
| Branch snapshots | Implemented | Capture copies all guest RAM into a memfd. Children restore with private copy-on-write mappings. The full path is not zero-copy. |
| Campaign PC coverage | Implemented baseline signal | Campaigns retain guest-PC samples at deterministic exits and barriers. This is not application basic-block or edge coverage. |
| C application coverage | Implemented, awaiting native evidence | The packaged GCC frontend emits build-scoped module-relative basic-block identities. Campaign guidance, timelines, replay, comparison, evaluation, and reports retain them. Tutorial 31 is the public runtime path; edge and other-language instrumentation remain absent. |
| Single-step collector | Implemented reference signal | Small guests can yield an instruction-address set. MMIO limitations remain, and the set is not source-level coverage. |
| Replay | Implemented for locked recorded fields | Replay re-executes supported bundles and compares retained fingerprints, serial output, actions, properties, and device evidence. It does not establish unrecorded state equality. |
| Campaign comparison | Implemented offline diff | `compare` finds the first recorded difference between two bundles. “Causal divergence” was removed from CLI output, tests, and tutorials. |
| Causality analysis | Proposed | Counterfactual re-exploration from checkpoints is Priority 3 in `PLAN.md`. |
| Bounded C thread scheduling | Implemented, awaiting native evidence | The packaged GCC C frontend controls pthreads at instrumented application basic blocks and retains ordered runnable-set choices through replay, comparison, and reports. It is limited to 32 threads, 8,192 decisions, and `pthread_join`; it is not general Linux thread/process scheduling. |
| Command overlap | Implemented, awaiting published runtime evidence | Named image commands can span whole-topology checkpoints and multiple simulated-network services. Launch and completion-observation order plus stable operation IDs are replay fields; this is not arbitrary thread scheduling. Tutorial 30 defines the native evidence workload; the workflow definition is not proof that it ran. |
| Public evaluation fixture | Format example only | Tutorial 13 and the current replicated-counter material do not independently prove execution without complete replay artifacts. Hashes prove retained bytes only. |
| Antithesis parity | Not claimed | The comparison document identifies narrower coverage support, missing general scheduling, hosted scale, and counterfactual investigation. No performance parity claim is supported. |

## Tutorial disposition

- Every tutorial README names its directory as the working context and uses a
  published Theseus binary, SDK crate, or runtime image.
- Tutorials 1 and 2 use normal Linux random devices. Tutorial 3 is the first
  SDK example. Tutorial 4 explains UART versus the Linux TTY interface and
  separates simulated input from Raspberry Pi hardware.
- Orchestration-only `run.sh` and `run-in-runtime.sh` files were removed.
  Tutorials 1, 2, and 4 retain `run.sh` because it is the actual low-level
  Firecracker boot/UART harness; their READMEs require the user to inspect it
  before execution.
- Tutorials 5–13 expose build, execution, expected-failure, inspection,
  replay, and cleanup commands directly in their READMEs.
- Container and Compose tutorials 14–32 expose host image builds, interactive
  runtime entry, locked input preparation, execution, evidence inspection,
  replay, and optional cleanup as separate steps.
- Generated bundles remain available until the user explicitly runs cleanup.

## Corrections made

- Removed historical test-count claims and statements that every layer had
  already been proven on hardware.
- Replaced “ground-truth coverage” with precise PC-signal terminology.
- Replaced causal language in the comparison report with chronological
  recorded-divergence language.
- Qualified entropy, clock, checkpoint, certification, evaluation, and
  platform claims in root and reference documentation.
- Made native evidence publication pair-atomic and independently checkable by
  the published CLI instead of relying on filenames and shell searches.
- Replaced the obsolete milestone ledger with an evidence-driven roadmap.
- Added CI checks for tutorial structure and high-risk documentation wording.

## Re-audit trigger

Update this audit when a PR changes runtime guarantees, supported platforms,
bundle evidence, coverage identity, scheduling control, or causality analysis.
Do not mark a proposed row demonstrated until its exact public artifact and
retrieval instructions are available.
