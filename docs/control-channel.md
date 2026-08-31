# The control channel

The control channel is the guest↔host door: the deterministic environment
injects events and the guest reports lifecycle and markers. Two transports
exist; the protocol contract lives in [`sdk/`](../sdk/) and is shared by
the host device and guest code. See [exploration.md](exploration.md) for
how the explorer drives it.

## MMIO transport (bare metal / any guest)

`TheseusDevice` is always attached on the MMIO bus at a fixed platform slot
(`0x40003000` on aarch64; right after the boot timer on x86_64). Register
map (8 bytes):

| Offset | Name | Dir | Meaning |
|---|---|---|---|
| 0 | `MAGIC` | R | ASCII `"THES"` (device detection; per-byte readable) |
| 4 | `STATUS` | R | bit 0: host→guest events pending |
| 5 | `EVENT` | R | pop one byte from the host→guest FIFO |
| 6 | `COMMAND` | W | guest command (`0x01` = setup complete) |
| 7 | `LOG` | W | guest marker byte into the host event log |

No IRQ, no ACPI/FDT entry: the guest polls with raw loads/stores. The
device is not snapshotted (the FIFO is transient); it is re-attached on
every snapshot restore.

## Serial transport (Linux guests, no driver)

For real kernels without devmem/UIO, the channel runs over the console
UART. Markers are structured lines out, events are lines in:

- guest→host: `THES:M:xx` (marker byte, hex)
- host→guest: `THES:E:xx` (event byte, hex)

Non-matching lines (kernel logs) are skipped. `sdk::linux::TtyChannel`
implements this; `e2e/agent` is a working example that runs as `/init` on a
stock kernel.

## Protocol rounds

The reactive workload protocol (used by the explorer and the bare-metal
test guests):

1. Guest emits boot marker (`0x42`), then `SETUP_COMPLETE` (`0x01`).
2. Host pushes event bytes, then the terminator (`0x00`).
3. Guest echoes each event as a marker; on terminator it emits the done
   marker (`0xFF`) and loops back to waiting.

The loop-forever shape matters: a timeline branched between rounds resumes
into the guest's wait state, so children of a branch point engage in new
rounds. Terminator is `0x00`, done is `0xFF` — event payloads may be
anything else; branch suffixes are `index + 1` so they never collide with
the terminator.

## SDK usage

Bare metal (no_std):

```rust
let door = unsafe { ControlChannel::new(0x4000_3000 as *mut u8) };
assert!(door.detect());
door.marker(MARKER_BOOT);
door.command(CMD_SETUP_COMPLETE);
door.event_round(); // echo events, mark done
```

Linux (std):

```rust
let mut ch = TtyChannel::console()?;
ch.marker(MARKER_BOOT)?;
ch.event_round()?;
```
