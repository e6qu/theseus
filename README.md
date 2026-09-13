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
- Deterministic simulated network and memory-backed storage faults.
- UART operations for unmodified Linux services and an optional guest SDK.
- Compose campaigns with bounded operations, faults, serial properties,
  minimization, replay, and offline reports.
- In-memory branch capture and private copy-on-write child mappings.
- Container-image conversion for ordinary Linux service images.
- SHA-addressed Linux runtime images for amd64 and arm64, plus published CLI
  binaries for Linux amd64/arm64 and macOS arm64.

## Important limits

- Linux and KVM are required to execute microVMs. The macOS binary can plan,
  validate, and inspect retained evidence, but cannot run Firecracker.
- Virtual counters free-run between exit-counted tick boundaries. Theseus does
  not promise instruction-exact virtual time.
- Campaign “coverage” is sampled guest vCPU instruction pointers. It is not
  application basic-block or edge coverage.
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
runner. Native KVM certification is a separate manually triggered self-hosted
amd64/arm64 workflow; it produces evidence only when it actually runs and its
certificate is retained.

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
