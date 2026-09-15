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
pushes, never on `main`). One job on pinned Ubuntu 24.04:

1. `cargo check --workspace` (the fork)
2. `cargo check` for `sdk/`, `engine/`, `orchestrator/`
3. `cargo test` for `engine/` and the KVM-free filters of
   `orchestrator/` (`branch::tests`, `orchestrator::tree`)
4. The deterministic `vmm` unit suites selected by exact module paths so the
   suite stays independent of `/dev/kvm` and `/dev/net/tun`.
5. The GCC C coverage and scheduling frontends, LLVM C/C++/Rust edge coverage,
   and Go main-module block coverage, including Cargo and Go dependency graphs,
   stable rebuild and DSO identities, locked catalog and pre-boot symbol
   validation, automatic source joins, deterministic pthread interleavings,
   controlled mutex and condition-variable blocking, a lost-update witness,
   and versioned records, without starting a VM.
6. Bounded thread-schedule expansion, locked schedule cases, invalid-search
   rejection, campaign selection, reporting, and tutorial structure.

KVM-backed tests (branch boots, explorer, coverage) are intentionally not
in CI — run them locally in the privileged container.

## Runtime certification

The `certify deterministic runtime` workflow first resolves the full commit
from a published 12-character SHA release. It verifies the signed release
input record and both native image attestations on a hosted runner. A successful
release starts the amd64 job on GitHub's native Ubuntu runner automatically.
Dispatch the workflow with `architecture=arm64` or `architecture=both` when an
arm64 Linux runner labelled `self-hosted`, `ARM64`, and `kvm` is online. There
is no emulated substitute for native KVM execution.

```sh
gh workflow run certify-deterministic-runtime.yml \
  -f tag="$TAG" -f architecture=arm64
```

Each worker runs Tutorial 11's fixed topology twice and Tutorial 30's
three-service lost-update campaign. It verifies the required partition and
recovery, minimizes the failure, and replays it. The same worker also executes
Tutorials 14, 31, 33, 35, and 41 with the published runtime: an ordinary
container, C coverage guidance, bounded thread-schedule search, pthread
synchronization, and exact per-vCPU and machine-wide KVM-exit replay. It retains every plan,
locked bundle, campaign inventory, report, minimization, replay result, serial
log, host identity, and digest-pinned runtime identity in an inventoried
validation archive. The released CLI also
renders the report, compares the coverage campaign with its replay, captures
an offline evaluation, evaluates its lock, minimizes the schedule failure, and
replays every retained path.

The final hosted job validates the certificate, counterexample, and validation
archive with the released CLI before it attests and uploads them. The index can
name one or both supported architectures, so consumers never have to infer the
certification scope from absent files. The workflow file alone is not proof;
the indexed assets must exist and verify on the named release.

The certificate is evidence for the strict `linux-kvm-simulated-io-v1`
profile, not a claim about tap networking, host-backed disks, or every clock
read inside one exit-counted quantum. Version 2 requires nonempty per-vCPU
KVM-exit evidence, version 3 adds a machine-wide stream, and version 4 requires
the complete bounded stream used to gate active replay.

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
