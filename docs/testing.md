# Testing

How to run everything. See [architecture.md](architecture.md) for the crate
layout and [exploration.md](exploration.md) for what the tests prove.

## Dev loop on macOS (aarch64)

Everything runs in Docker: a privileged aarch64 Linux container gets
`/dev/kvm` on Apple Silicon.

```sh
IMG=rust:1.97.0-bookworm
docker run --rm --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus/firecracker $IMG sh -c \
  "apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev && \
   cargo test -p vmm --lib -- --test-threads=1"
```

- **vmm suite**: `cargo test -p vmm --lib` in `firecracker/`. The exact test
  count changes as the fork evolves; a count is not runtime evidence.
- **Theseus crates**: `cargo test` in `sdk/`, `engine/`, `orchestrator/`
- **x86_64 without KVM**: cross-compile and run under `qemu-user`
  (only the pure unit suites pass there; KVM-backed tests need real KVM).

## End-to-end proofs

`e2e/run.sh` (run inside the privileged container, repo mounted at
`/theseus`) performs live checks on real KVM:

1. Stock-kernel `/dev/random` and `/dev/urandom` observations, explicitly
   informational because the stock CSPRNG mixes guest timing.
2. MMIO control channel from a bare-metal guest.
3. Linux serial control channel from a static musl guest agent.

Build first: `cargo build -p firecracker` in `firecracker/`, then
`sh e2e/run.sh`.

## CI

`.github/workflows/ci.yml` triggers on `pull_request` only (never on raw
pushes, never on `main`). One job on `ubuntu-latest`:

1. `cargo check --workspace` (the fork)
2. `cargo check` for `sdk/`, `engine/`, `orchestrator/`
3. `cargo test` for `engine/` and the KVM-free filters of
   `orchestrator/` (`branch::tests`, `orchestrator::tree`)
4. The deterministic `vmm` unit suites selected by exact module paths so the
   suite stays independent of `/dev/kvm` and `/dev/net/tun`.

KVM-backed tests (branch boots, explorer, coverage) are intentionally not
in CI — run them locally in the privileged container.

## Runtime certification

The `certify deterministic runtime` workflow defines the requested real-KVM
support matrix.
It runs only on self-hosted Linux metal labelled `kvm`: one `X64` worker and
one `ARM64` worker. When those workers are available and the workflow is
dispatched with a published SHA, each worker pulls its
native runtime image, runs Tutorial 11's fixed topology twice, attests the
resulting `certificate.json`, and attaches it to that release. The workflow
file alone is not proof that either job ran.

The certificate is evidence for the strict `linux-kvm-simulated-io-v1`
profile, not a claim about tap networking, host-backed disks, or every clock
read inside one exit-counted quantum.

## Publish a failure from CI

Keep the failed replay directory as the durable reproduction artifact, then
render the same evidence for people and CI dashboards. These commands do not
need KVM because they only read the completed bundle:

```sh
theseus report --format markdown theseus-compose-campaign >> "$GITHUB_STEP_SUMMARY"
theseus report --format junit --output theseus-results.xml theseus-compose-campaign
theseus report --format json --output theseus-results.json theseus-compose-campaign
```

Upload `theseus-compose-campaign/`, `theseus-results.xml`, and
`theseus-results.json`. The Markdown names the exact locked replay command;
the JUnit file exposes each property as a test case; the JSON document uses
the stable `theseus-report-v1` format for automation.
