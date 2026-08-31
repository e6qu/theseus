# theseus-engine

Leaf deterministic components used *by* the Firecracker VMM. Depends only
on [`sdk/`](../sdk/) — never on `vmm` (no dependency cycles). The `vmm`
crate depends on this crate and re-exports its modules at their old
in-crate paths. AGPL-3.0-or-later (see [../LICENSE](../LICENSE)).

## Modules

| Module | What it does |
|---|---|
| `detrng` | One seeded ChaCha stream per process for every host-side guest-visible randomness source (FDT `rng-seed`, vmgenid, MMDS token keys, dumbo TCP ISNs) |
| `vclock` | The tick-stepped virtual clock: time as a pure function of tick count, plus counter conversions (x86_64 TSC, aarch64 CNTVCT) and snapshot state |
| `simnet` | The simulated network backend: loopback FIFO, total partition, seeded per-frame drops (`drop_ppm`) |
| `door` | `TheseusDevice`: the MMIO control channel (magic/status/event-FIFO/command/log registers) |

## Documentation

- [The determinism model](../docs/determinism.md) — what these components close
- [The control channel](../docs/control-channel.md) — the door's register map and protocol
- [Architecture](../docs/architecture.md) — why this crate is a leaf
