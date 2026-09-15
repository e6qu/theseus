# Theseus

Theseus is an open deterministic-testing runtime for Linux services. It boots
ordinary service artifacts in Firecracker microVMs, controls selected inputs
and faults, explores bounded timelines, and retains replay bundles for failed
properties.

The project is under active development. Its strongest current path is a
Linux/KVM Compose campaign using simulated I/O and explicit workload input.
The exact guarantees and known gaps are documented below; source code or a
passing unit suite alone is not treated as runtime proof.

## What works today

- Seeded virtio entropy and host-side random sources.
- A Linux kernel module that installs the Theseus seed into the normal Linux
  CRNG, allowing `/dev/random` and `/dev/urandom` tutorials to replay.
- Exit-counted virtual-time quanta on amd64 and arm64.
- Deterministic simulated network and memory-backed storage faults, including
  a topology-derived profile for service lifecycle and asymmetric links.
- UART operations for unmodified Linux services and an optional guest SDK.
- Compose campaigns with bounded operations, faults, serial properties,
  minimization, replay, and offline reports.
- Test-command lifecycle roles for first, parallel, serial, singleton,
  anytime, eventually, and final work, with invalid combinations removed from
  the generated corpus.
- Automatic discovery of Antithesis-compatible test templates under
  `/opt/antithesis/test/v1`, including explorer-selected command concurrency,
  one-template-per-timeline selection, eventual-command termination, and a
  quiet terminal fault window.
- Named image commands that remain in flight across operation checkpoints,
  with replayed launches and completion-observation order.
- Expected-counterexample commands that fail closed unless the named campaign
  property is retained as failed.
- In-memory branch capture and private copy-on-write child mappings.
- Container-image conversion for ordinary Linux service images.
- GCC C and Go basic-block coverage plus LLVM C/C++/Rust edge coverage with
  build-scoped identities retained through campaign guidance, replay,
  comparison, and reports. LLVM shared libraries keep their own module
  identity; CLI builds cover selected Cargo graphs and selected Go commands'
  imported main-module packages. Packaged tools validate and symbolize them.
- Bounded GCC C pthread scheduling at application basic-block boundaries, with
  explicit schedules and runnable-set decisions retained and replay-checked;
  default mutexes and untimed condition variables update the runnable set.
- SHA-addressed Linux runtime images for amd64 and arm64, plus published CLI
  binaries for Linux amd64/arm64 and macOS arm64.

## Important limits

- Linux and KVM are required to execute microVMs. The macOS binary can plan,
  validate, and inspect retained evidence, but cannot run Firecracker.
- Virtual counters free-run between exit-counted tick boundaries. Theseus does
  not promise instruction-exact virtual time.
- Application coverage uses explicit build frontends and coverage catalogs declared
  under each service's `x-theseus` configuration; it does not transparently
  instrument arbitrary existing images. Declared manifests and symbols are
  locked into replay bundles and joined into reports. `theseus coverage cargo`
  covers one selected Rust binary and its static Rust target dependencies;
  `theseus coverage go` covers one Linux Go command and imported source
  packages in its main module. Go external modules and CGO, Rust dynamic
  libraries, Java, JavaScript, and .NET instrumentation remain unsupported.
- Thread scheduling is a bounded GCC C path, not general Linux scheduling. It
  supports 32 pthreads and 8,192 decisions, and controls joins, default mutex
  locking, and untimed condition waits/signals/broadcasts. Timed waits,
  cancellation, direct futex use, blocking I/O, and processes remain outside
  this profile.
- `compare` finds the first difference in two recorded histories. It does not
  perform counterfactual re-exploration or prove causality.
- Capturing a branch copies guest RAM into a memfd. Restored children then use
  private copy-on-write mappings; the complete capture/restore path is not
  zero-copy.
- A runtime certificate applies only to the exact recorded plan, artifacts,
  architecture, and supported simulated-I/O profile.

See [PLAN.md](PLAN.md) for the current roadmap and
[the claim audit](docs/audits/2026-09-runtime-and-docs.md) for the documentation
review.

## Start with the tutorials

The [tutorial index](docs/tutorials/) starts with CLI and service-facing
examples:

1. Replay ordinary Linux `/dev/random` and `/dev/urandom` reads.
2. Select a deterministic random stream with a seed.
3. Add the optional SDK control channel to a bare-metal guest.
4. Record and replay serial/TTY input like a Raspberry Pi sensor reading.
5. Move to multi-service Compose campaigns, faults, reports, and container
   images.
6. Find and minimize a lost update by overlapping ordinary image commands.
7. Reproduce the lost update across three unmodified image services while
   exercising a required network partition, recovery, and the combined Compose
   runtime contract.
8. Instrument an ordinary C command and guide a campaign with stable
   application basic-block identities.
9. Reproduce a pthread lost update from explicit, replay-checked scheduling
   decisions.
10. Search bounded pthread schedule patterns, find a lost update, minimize it,
    and replay the locked failing case.
11. Grow pthread schedule prefixes from observed runnable sets instead of
    declaring candidate patterns in advance.
12. Control pthread mutex and condition-variable blocking and retain each
    synchronization transition with stable object identities.
13. Give a plain C command bounded choices, search their combinations with
    unified feedback, and replay the exact choice records.
14. Instrument C++, a dynamically loaded library, or Rust with LLVM edge
    coverage, resolve a reached source location, and replay the edge set.
15. Instrument a Go command and its imported main-module packages, then report
    and replay source-associated basic blocks.
16. Package ordinary commands in Antithesis-compatible test-template
    directories and let Theseus discover, select, overlap, stop, and replay
    them.

Each tutorial directory is its own working directory and complete input
context. Runnable tutorials use published Theseus images or binaries, not a
checkout-relative build artifact. The README exposes the commands and expected
observations step by step.

## Run a container service

Theseus accepts a Docker archive as `guest.image`. The published Linux runtime
contains `theseus-image`, which flattens the image into a bootable initramfs
and locks the adapter, kernel, Firecracker binary, image bytes, and resolved
launch contract for replay.

```toml
[runtime]
firecracker = "work/runtime/firecracker"
image_adapter = "work/runtime/theseus-image"

[guest]
kernel = "work/guest/vmlinux"
image = "work/service.tar"
```

No guest SDK is required for HTTP, command, health-check, or serial workloads.
Use the SDK only when explicit in-guest markers or control events improve the
property contract.

## Repository layout

| Path | Purpose |
|---|---|
| [`cli/`](cli/) | manifests, campaigns, replay, comparison, evaluation, reports |
| [`topology-runner/`](topology-runner/) | multi-service KVM execution and certification |
| [`image-runner/`](image-runner/) | commands and probes inside converted images |
| [`explorer-runner/`](explorer-runner/) | single-guest branching execution |
| [`orchestrator/`](orchestrator/) | branch capture, timelines, OCI conversion, PC collection |
| [`engine/`](engine/) | seeded entropy, virtual clock, simulated devices, control door |
| [`sdk/`](sdk/) | optional guest control-channel library |
| [`firecracker/`](firecracker/) | Apache-2.0 Firecracker fork and marked Theseus changes |
| [`docs/`](docs/) | tutorials, behavior contracts, limitations, and audits |

## Development checks

The pull-request workflow runs compilation, unit tests, deterministic subsystem
tests, tutorial structure checks, and release-input checks on an amd64 GitHub
runner. Every successful SHA release then starts its amd64 KVM certification.
Arm64 certification can be dispatched on an arm64 KVM runner. Each run resolves
and verifies the published SHA before it executes and attaches an indexed,
CLI-verifiable evidence set to that release. The index says exactly which native
architectures were certified; a workflow definition alone is not evidence.

```sh
cargo test --manifest-path cli/Cargo.toml --locked
cargo test --manifest-path topology-runner/Cargo.toml --locked
cargo test --manifest-path image-runner/Cargo.toml --locked
cargo test --manifest-path explorer-runner/Cargo.toml --locked
```

See [docs/testing.md](docs/testing.md) for the complete development and
runtime-validation paths.

## Documentation

- [Architecture](docs/architecture.md)
- [Determinism and replay limits](docs/determinism.md)
- [Exploration and coverage](docs/exploration.md)
- [CLI and test-directory reference](cli/README.md)
- [Comparison with Antithesis and related tools](docs/comparison.md)
- [Tutorials](docs/tutorials/)

## License

Theseus-authored code is AGPL-3.0-or-later. The `firecracker/` subtree remains
Apache-2.0; see [firecracker/README-THESEUS.md](firecracker/README-THESEUS.md)
for provenance and the marked fork boundary.
