# e2e — live end-to-end proofs

Boots real microVMs on real KVM and asserts the properties the unit tests
can't reach. AGPL-3.0-or-later (see [../LICENSE](../LICENSE)).

## What it proves

`run.sh` runs four proofs inside a privileged Linux container (repo mounted
at `/theseus`):

1. **Seeded entropy**: three boots with the CI kernel + a tiny initramfs
   (static C init). Seed 42 twice → byte-identical `/dev/hwrng`; seed 1337
   → different.
2. **Known leak note**: `/dev/urandom` diverges even on same-seed boots —
   the guest kernel's CSPRNG mixes timing jitter (informational, not a
   failure).
3. **MMIO control channel**: a 216-byte bare-metal guest
   (`firecracker/.../theseus_guest.S`) reads the magic register and issues
   setup-complete + a log marker; the host drains exactly those events.
4. **Serial control channel**: `agent/` (static musl Rust binary using
   `theseus_sdk::linux`) does a full marker/event round trip over the
   serial console on a stock kernel.

## Running it

```sh
# inside the privileged aarch64 Linux container, repo at /theseus
cargo build -p firecracker        # in firecracker/
sh e2e/run.sh
```

The guest kernel is downloaded from the public Firecracker CI bucket on
first run. `agent/` is rebuilt each run (target is pinned to
`aarch64-unknown-linux-musl`).

## Documentation

- [Testing](../docs/testing.md) — the full dev loop
- [The control channel](../docs/control-channel.md) — what proofs 3 and 4 exercise
