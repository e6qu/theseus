# Advanced guide: Control virtual time

Use virtual time for event-driven timeout, retry, and election logic. It
advances the guest clock after a fixed number of guest-visible exits.

Enable it in the machine configuration:

```json
"virtual_time": { "tick_ns": 1000000, "exits_per_tick": 64 }
```

`tick_ns` is the amount of virtual time per tick. `exits_per_tick` chooses
when the next tick occurs.

## Run the check

Use the Linux+KVM container from tutorial 1, then run:

```sh
cd /theseus/orchestrator
cargo test --lib spawn::tests::test_guest_virtual_time_is_reproducible
```

The test verifies three things:

- virtual time starts near zero;
- two virtual-time runs are close; and
- two host-time runs differ.

Do not branch on an exact guest counter value between ticks: counter reads do
not exit the VM, so a small host-time tail remains. Event-driven code replays
at the tick boundary.

See [determinism](../../determinism.md) for the full boundary.
