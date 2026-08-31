# Tutorial: The control channel

## Objective

Boot a bare-metal guest that detects the Theseus control device by
reading the bytes `"THES"` from its magic register, feed it two event
bytes from the host (`0x90` and the terminator `0x00`), and read back the
markers it reports in response — including its own classification of the
event (`0xB0`) and a done marker (`0xFF`). You will have a working
two-way channel that needs no driver, no interrupts, and no setup.

## The problem

A test engine that runs virtual machines from the outside needs a
deterministic way to talk to the guest: to inject inputs and faults, and
to hear what the guest did. Ordinary channels are unsuitable — a network
is nondeterministic and needs drivers; a serial console is text. Theseus
uses a tiny memory-mapped device instead: a few bytes of register space
the guest reads and writes directly, called the *control channel* or
*door*. Three terms to know: an *event* is a byte the host sends to the
guest; a *marker* is a byte the guest reports back; the guest *detects*
the device by reading the ASCII bytes `"THES"` from the magic register.

The device exposes five byte registers:

| Offset | Name | Direction | Meaning |
|---|---|---|---|
| 0 | `MAGIC` | read | ASCII `"THES"` — device detection |
| 4 | `STATUS` | read | bit 0: host→guest events pending |
| 5 | `EVENT` | read | pop the next event byte |
| 6 | `COMMAND` | write | guest commands (`0x01` = setup complete) |
| 7 | `LOG` | write | guest marker bytes |

## Prerequisites

You need a Linux machine with KVM (the kernel virtual machine feature).
On Apple Silicon macOS, a privileged aarch64 Docker container provides it.
From the repository root, start it like this:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0 bash
```

Inside the container, install the build dependencies and build once:

```sh
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev
cd /theseus/firecracker && cargo build -p firecracker
```

## Step 1 — Watch a guest detect the door

The repository contains a 216-byte bare-metal guest (a program that runs
without an operating system) which reads the magic register, prints what
it found, writes a boot marker, and signals setup complete. Run it:

```sh
cd /theseus/orchestrator
cargo test --lib spawn::tests::test_guest_control_channel_roundtrip
```

You will see the test pass. The guest printed `magic=THES` to the
console, and the host drained exactly two items from the device: the boot
marker (`0x42`) and the setup-complete command (`0x01`).

## Step 2 — Drive events into the guest and read its answer

Now the full round trip: the host pushes event bytes into the device
before the guest boots; the guest pops them, classifies them, and reports
markers back. Run:

```sh
cargo test --lib spawn::tests::test_rust_guest_event_paths
```

The test pushed two events (`0x90`, then the terminator `0x00`) and
drained this exact sequence:

```
boot marker (0x42) → setup complete (0x01) → path marker (0xB0) → echo (0x90) → done (0xFF)
```

The `0xB0` is the guest's own classification of the event — proof the
guest received the host's input, decided something about it, and reported
its decision back to the host.

## Step 3 — See the guest code behind it

The guest uses a five-method driver. The complete pattern:

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

A Linux guest (which cannot touch raw device memory without a driver)
runs the same protocol as text lines over the serial console — the
`theseus_sdk::linux` module implements it, and `e2e/agent` is a working
static binary that does a full round trip on a stock kernel.

## What you have now

A deterministic two-way channel: the host feeds a guest a seeded stream
of inputs, and the guest reports its behavior as markers — no drivers, no
interrupts, no setup. Every experiment in this project is built on it.

## Further reading

- [control-channel.md](../control-channel.md) — the full register map and
  protocol rounds
- [terminology.md](../terminology.md) — definitions of event, marker, and
  the rendezvous protocol
