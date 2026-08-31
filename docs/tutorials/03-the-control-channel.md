# Tutorial: Give the guest a voice

## Objective

Give a guest a deterministic way to report results to the host and to
accept commands from it: the guest detects a device by reading `"THES"`,
the host pushes two event bytes into it, and the guest reports back its
boot, its readiness, its decision about the input, and its completion —
all as marker bytes the host drains. When you finish, you will have the
channel every Theseus test uses.

## The problem

A virtual machine is a black box. To test it from outside you need to
know what happened inside — did it reach your checkpoint, did it take the
path you injected — and you need to feed it inputs deterministically. The
control channel solves both with a memory-mapped device: five byte
registers the guest reads and writes directly. No driver, no interrupts,
no setup. The guest polls a status byte; the host watches an event log.

| Offset | Name | Direction | Meaning |
|---|---|---|---|
| 0 | `MAGIC` | read | ASCII `"THES"` — device detection |
| 4 | `STATUS` | read | bit 0: events pending |
| 5 | `EVENT` | read | pop the next event byte |
| 6 | `COMMAND` | write | guest commands (`0x01` = setup complete) |
| 7 | `LOG` | write | guest marker bytes |

## Setup

You need a Linux machine with KVM. On Apple Silicon macOS, a privileged
aarch64 Docker container provides it. From the repository root:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0 bash
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev
cd /theseus/firecracker && cargo build -p vmm
```

## Step 1 — The guest detects the door

A bare-metal guest (a 216-byte program that runs without an operating
system) reads the magic register and writes its boot and setup markers.
Run it:

```sh
cargo test -p vmm --lib spawn::tests::test_guest_control_channel_roundtrip
```

It passes. The guest printed `magic=THES` on the console; the host
drained exactly `[0x42, 0x01]` from the device: booted, ready.

## Step 2 — Push events and read the answer

The host pushes `0x90` and the terminator `0x00` into the device before
boot; the guest pops them, classifies `0x90` (it takes the high path and
reports `0xB0`), echoes the event, and reports done. Run:

```sh
cargo test -p vmm --lib spawn::tests::test_rust_guest_event_paths
```

The host drains:

```
[boot 0x42, setup 0x01, high-path 0xB0, echo 0x90, done 0xFF]
```

The `0xB0` proves the guest received the host's input, decided something
about it, and reported the decision.

## Step 3 — The guest side, complete

This is the whole driver the guest uses:

```rust
let door = unsafe { ControlChannel::new(DOOR_ADDRESS) };
door.detect();              // reads MAGIC, expects "THES"
door.marker(0x42);          // report: booted
door.command(0x01);         // report: setup complete
loop {
    door.wait_events();     // spin while STATUS shows nothing pending
    let event = door.pop_event();
    if event == 0x00 {      // terminator
        door.marker(0xFF);  // report: round done
    } else {
        door.marker(event); // echo the input back
    }
}
```

Linux guests use the same protocol as text lines over the serial console
(`theseus_sdk::linux`) — no driver required on a stock kernel.

## What you have now

A deterministic two-way channel: the host feeds a guest a seeded stream
of inputs, the guest reports its behavior as markers. Every Theseus test
runs on this channel.

## Further reading

- [control-channel.md](../control-channel.md) — the full register map and
  protocol rounds
- [terminology.md](../terminology.md) — event, marker, rendezvous
