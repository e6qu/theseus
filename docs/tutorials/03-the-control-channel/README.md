# Tutorial 3: Instrument a guest with `theseus-sdk`

## Objective

The first two tutorials controlled a guest entirely from the service API.
Now add the smallest possible guest-side contract: `theseus-sdk` lets the
guest report lifecycle markers and consume deterministic events. This is the
point at which Theseus can judge your program, not merely boot it.

## The SDK contract

The SDK wraps a tiny control device. Your guest reads the device magic, marks
itself ready, then consumes events and emits outcomes:

```rust
let door = unsafe { ControlChannel::new(DOOR_ADDRESS) };
door.detect();
door.marker(0x42);          // booted
door.command(0x01);         // ready for a test round
loop {
    door.wait_events();
    let event = door.pop_event();
    if event == 0x00 {
        door.marker(0xFF);  // round complete
    } else {
        door.marker(event); // application-defined outcome
    }
}
```

The complete Rust guest is
[`theseus_guest_rs/main.rs`](../../../firecracker/src/vmm/src/test_utils/mock_resources/theseus_guest_rs/main.rs).
It is deliberately small, but it is an SDK client rather than a test-only
host API example.

## Setup

You need Linux with KVM. On Apple Silicon macOS, start the same privileged
aarch64 container used in the earlier tutorials:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0-bookworm bash
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev
```

## Step 1 — Check the lifecycle signal

The host boots the SDK guest and drains its first two messages:

```sh
cargo test --manifest-path /theseus/orchestrator/Cargo.toml --lib \
  spawn::tests::test_guest_control_channel_roundtrip
```

It observes `[0x42, 0x01]`: booted, then ready.

## Step 2 — Send an event and judge the result

The next test queues `0x90` and the terminator before boot. The SDK guest
takes its high path, reports it, echoes the event, and finishes the round:

```sh
cargo test --manifest-path /theseus/orchestrator/Cargo.toml --lib \
  spawn::tests::test_rust_guest_event_paths
```

The drained marker stream is:

```
[boot 0x42, setup 0x01, high-path 0xB0, echo 0x90, done 0xFF]
```

## What you have now

The service can provide inputs; your program can state the outcomes that
matter. That is enough to write a property and let a fault schedule expose a
failure, which is the next tutorial.

## Further reading

- [control-channel.md](../../control-channel.md) — register map and protocol
- [the SDK README](../../../sdk/README.md) — bare-metal and Linux transports
