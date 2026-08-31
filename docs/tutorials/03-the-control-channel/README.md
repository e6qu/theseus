# Tutorial 3: Instrument a guest with `theseus-sdk`

The service can now supply inputs. Add `theseus-sdk` so the guest can report
its state and result back to the host.

## 1. Add the guest contract

Use a `ControlChannel` to report boot and readiness, consume events, and
report the result:

```rust
let door = unsafe { ControlChannel::new(DOOR_ADDRESS) };
door.detect();
door.marker(0x42);          // booted
door.command(0x01);         // ready
loop {
    door.wait_events();
    let event = door.pop_event();
    door.marker(if event == 0x00 { 0xFF } else { event });
}
```

The complete example is
[`theseus_guest_rs/main.rs`](../../../firecracker/src/vmm/src/test_utils/mock_resources/theseus_guest_rs/main.rs).

## 2. Run the lifecycle check

Use the Linux+KVM container from tutorial 1:

```sh
cargo test --manifest-path /theseus/orchestrator/Cargo.toml --lib \
  spawn::tests::test_guest_control_channel_roundtrip
```

The host receives `[0x42, 0x01]`: booted, then ready.

## 3. Send an event

```sh
cargo test --manifest-path /theseus/orchestrator/Cargo.toml --lib \
  spawn::tests::test_rust_guest_event_paths
```

The host receives:

```text
[boot 0x42, setup 0x01, high-path 0xB0, echo 0x90, done 0xFF]
```

Markers are the property surface for your guest. Use them in the next tutorial
to catch a failure under a fault schedule.

See [the control-channel reference](../../control-channel.md) and
[the SDK README](../../../sdk/README.md).
