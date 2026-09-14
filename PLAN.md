# Theseus roadmap

## Product objective

Run ordinary Linux services under controlled inputs and faults, discover a
failure, and let another person reproduce and investigate it from published
artifacts. Measure progress by demonstrated runtime behavior and retained
evidence, not by implemented types, accepted syntax, or test counts.

## Current baseline

Theseus has a Linux/KVM runtime, Compose planning and execution, container
image conversion, simulated network and storage, exit-counted virtual time,
bounded campaign search, minimization, replay, reports, bundle comparison, and
an optional guest SDK. Image campaigns can keep named ordinary commands in
flight across whole-topology checkpoints and record when each completion is
observed. The distributed lost-update workload combines three image services,
simulated network traffic, a required partition/recovery path, and the
supported Compose runtime contracts in one replayable campaign. Required
campaign faults remain in every applicable schedule and survive minimization.
Native certification resolves a signed SHA release before execution, records
the host kernel, KVM API, digest-pinned runtime, and complete counterexample
inventory, and publishes only a dual-architecture evidence set that the
released CLI can verify offline.

The following limits define the honest baseline:

- Pull-request CI runs mostly environment-independent checks on an amd64
  GitHub runner. Native KVM behavior is evidence only when a matching runtime
  job actually ran and retained its certificate.
- Linux CSPRNG replay requires the matching published kernel and Theseus seed
  module. Seeded virtio entropy alone does not remove stock-kernel timing mix.
- Guest counters free-run within an exit-counted virtual-time quantum.
- Campaign execution signals are raw vCPU PCs sampled at exits and barriers,
  not application basic-block or edge coverage.
- `compare` finds differences between retained histories. It does not perform
  counterfactual experiments and must not claim causality.
- Branch capture copies guest RAM into a memfd. Restored children then use
  private copy-on-write mappings; capture is not zero-copy.
- The recorded public evaluation fixtures explain formats but do not contain
  complete, independently replayable runtime evidence.

## Evidence rules

Apply these rules to code, documentation, releases, and future PRs:

1. Label a capability as implemented, runtime-demonstrated, or proposed.
2. Treat a hash as proof of retained bytes only, never proof of execution.
3. State the architecture, kernel/module pair, runtime digest, plan, and I/O
   profile for KVM evidence.
4. Preserve the replay plan, locked runtime/workload artifacts, results, logs,
   operation boundaries, fault schedule, and property verdict for a public
   counterexample.
5. Keep tutorial directories self-contained. Run from the directory and use
   published Theseus artifacts only.
6. Put meaningful commands and expected observations in the tutorial README.
   Keep scripts only when they are actual workloads or reusable low-level
   tools, and make their contents reviewable before execution.
7. Do not restore historical P-number completion ledgers as active guidance.
   Git history is the record of completed work.

## Priority 0: prove the packaged runtime on a concurrent service failure

Deliver this as one coherent PR, with separately reviewable commits.

### Package the runtime that tests execute

- Run the combined Tutorial 30 contract on native amd64 and arm64 KVM: resource
  quantities, launch overrides, credentials, environment, configs, secrets,
  seeded volumes, read-only roots, tmpfs, health checks, service networking,
  in-flight commands, checkpoint restoration, minimization, and replay.
- Verify that the combined campaign's required partition, dropped UDP probe,
  heal, and successful HTTP probe are retained before its independent
  concurrency-dependent failure.
- Retain the attested fixed-plan certificate and portable counterexample
  archive on the exact SHA release. Leave a missing architecture explicitly
  unverified rather than substituting unit tests or a workflow definition.

### Find a concurrency-dependent failure

- Supply the self-hosted amd64 and arm64 KVM capacity, dispatch native
  certification for a published SHA, and verify that the indexed pair contains
  the passing sequential schedule, minimized distributed lost update, and
  successful locked replay. GitHub-hosted runners are not a substitute for
  native KVM and the repository currently has no registered native runners.
- Publish exact retrieval and attestation commands with the artifacts. Do not
  promote the implemented workload to demonstrated behavior until those
  release assets exist.

### Exit criteria

- The PR CI is green at the final commit.
- Both advertised Linux architectures have retained native KVM evidence for
  the exact candidate artifacts.
- A new user can retrieve the published artifacts, reproduce the minimized
  failure from its directory, inspect why the property failed, and replay it
  without undocumented repository inputs.
- `PLAN.md`, reference docs, comments, CLI wording, and tutorials describe the
  observed result and remaining limits consistently.

## Priority 1: application basic-block coverage

- Define stable process, module, and basic-block identities across ASLR and
  rebuilds.
- Instrument one bounded initial toolchain/language path.
- Preserve coverage identities in campaign bundles and reports.
- Measure runtime/storage overhead and demonstrate search improvement over
  marker, dirty-page, and PC-sampling baselines on the same workload budget.

Exit when a public workload produces replay-stable application coverage and a
controlled comparison shows that it finds a counterexample or useful state
that the existing signals miss.

## Priority 2: thread and process scheduling

- Introduce explicit replayable scheduling choices for a bounded Linux target.
- Record runnable entities and selected decisions with stable identities.
- Add deterministic perturbations and race examples below operation-level
  overlap.
- Carry scheduling state through checkpoint, minimization, and replay.

Exit when a real race can be found, minimized, and replayed from retained
artifacts without relying on host timing.

## Priority 3: counterfactual investigation

- Navigate a retained operation/fault/property timeline offline.
- Re-execute from an earlier checkpoint while changing one controlled event.
- Compare the resulting behavior with the original and label conclusions by
  the experiment actually performed.
- Never promote chronological difference to causal language automatically.

Exit when the tool can show, from reproducible experiments, that removing or
changing one event prevents or preserves a selected failure.

## Priority 4: exploration scale

- Bound checkpoint retention and garbage collection.
- Measure capture, restore, shared-page, and private-page costs on realistic
  multi-service topologies.
- Improve search scheduling only against public fixed-budget workloads.
- Pursue broader integrations or hosted execution after runtime evidence and
  artifact reproduction are reliable.

## Documentation maintenance

Every behavioral PR must update the relevant tutorial, reference contract,
code comments, and audit status in the same change. CI should reject hidden
tutorial wrappers, checkout-relative tutorial dependencies, stale causal or
coverage terminology, unsupported platform claims, and reintroduced references
to obsolete random-device examples.

Use Antithesis documentation as a capability reference for coverage,
scheduling, test templates, and counterfactual debugging. It is not evidence
that Theseus implements or matches those capabilities.
