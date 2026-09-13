# e2e — live end-to-end proofs

Boots real microVMs on real KVM and asserts the properties the unit tests
can't reach. AGPL-3.0-or-later (see [../LICENSE](../LICENSE)).

## What it checks

`run.sh` runs live checks inside a privileged Linux container (repo mounted
at `/theseus`):

1. **Stock-kernel entropy probe**: three boots read `/dev/random` and
   `/dev/urandom`. Their output is informational because this kernel mixes
   guest timing into its CSPRNG. Tutorials 1 and 2 use the matching published
   kernel module for random-device replay.
2. **MMIO control channel**: a 216-byte bare-metal guest
   (`firecracker/.../theseus_guest.S`) reads the magic register and issues
   setup-complete + a log marker; the host drains exactly those events.
3. **Serial control channel**: `agent/` (static musl Rust binary using
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
