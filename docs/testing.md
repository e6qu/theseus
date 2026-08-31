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

- **vmm suite** (761 tests incl. KVM-booting branch/explorer tests):
  `cargo test -p vmm --lib` in `firecracker/`
- **Theseus crates**: `cargo test` in `sdk/`, `engine/`, `orchestrator/`
- **x86_64 without KVM**: cross-compile and run under `qemu-user`
  (only the pure unit suites pass there; KVM-backed tests need real KVM).

## End-to-end proofs

`e2e/run.sh` (run inside the privileged container, repo mounted at
`/theseus`) performs four live proofs on real KVM:

1. Seed 42 booted twice → byte-identical `/dev/hwrng`; seed 1337 differs.
2. Note on guest-kernel CSPRNG jitter (known leak, informational).
3. MMIO control channel from a bare-metal guest.
4. Linux serial control channel from a static musl guest agent.

Build first: `cargo build -p firecracker` in `firecracker/`, then
`sh e2e/run.sh`.

## CI

`.github/workflows/ci.yml` triggers on `pull_request` only (never on raw
pushes, never on `main`). One job on `ubuntu-latest`:

1. `cargo check --workspace` (the fork)
2. `cargo check` for `sdk/`, `engine/`, `orchestrator/`
3. `cargo test` for `engine/` and the KVM-free filters of
   `orchestrator/` (`branch::tests`, `orchestrator::tree`)
4. The deterministic `vmm` unit suites (54 tests, exact module paths so the
   suite is green on runners without `/dev/kvm` or `/dev/net/tun`).

KVM-backed tests (branch boots, explorer, coverage) are intentionally not
in CI — run them locally in the privileged container.
