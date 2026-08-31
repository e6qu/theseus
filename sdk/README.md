# theseus-sdk

The shared protocol contract between the Theseus host and guest code —
`no_std`, so it links into bare-metal guests, and `std`-feature-gated
extras for host-side and Linux use. AGPL-3.0-or-later (see
[../LICENSE](../LICENSE)).

## Contents

| Module | What |
|---|---|
| root (`lib.rs`) | Control-channel contract: register offsets, `MAGIC`, commands (`CMD_SETUP_COMPLETE`), markers (`MARKER_BOOT`, `MARKER_DONE`, `EVENT_TERMINATOR`), and the bare-metal `ControlChannel` driver |
| `linux` (feature `std`) | `TtyChannel`: the serial-console transport (`THES:M:xx` / `THES:E:xx` lines) for Linux guests with no driver |
| `bus` (feature `std`) | Device bus primitives (`BusDevice`, `Bus`) moved out of `vmm::vstate` so the engine crate stays free of a `vmm` dependency |

## Used by

- `engine/door` — the host-side `TheseusDevice` (constants, registers)
- `firecracker` — the bus primitives via re-export
- the bare-metal test guests (`firecracker/src/vmm/src/test_utils/mock_resources/theseus_guest_rs/`)
- `e2e/agent` — the Linux serial-channel proof

## Documentation

- [The control channel](../docs/control-channel.md) — full protocol
- [Architecture](../docs/architecture.md) — where this crate sits in the dependency graph
