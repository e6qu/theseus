# Advanced guide: Control virtual time

Use exit-counted virtual time at the supported event boundaries. It advances
the guest counter after a fixed number of guest-visible exits; it does not
control Linux timers or make timeout, retry, and election logic deterministic
by itself.

Enable it in the machine configuration:

```json
"virtual_time": { "tick_ns": 1000000, "exits_per_tick": 64 }
```

`tick_ns` is the amount of virtual time per tick. `exits_per_tick` chooses
when the next tick occurs.

## Inspect the source-level check

This is a developer check, not a command inside a published runtime image.
From a Theseus source checkout on Linux with KVM, run:

```sh
cargo test --manifest-path orchestrator/Cargo.toml --lib spawn::tests::test_guest_virtual_time_is_reproducible
```

The test verifies three things:

- virtual time starts near zero;
- two virtual-time runs are close; and
- two host-time runs differ.

Do not branch on an exact guest counter value between ticks: counter reads do
not exit the VM, so a host-time tail remains. Replay checks the retained exit,
interrupt, input, and clock-jump sequence; any changed decision fails.

For a self-contained published-artifact workflow, follow
[ready-checkpoint replay](../../tutorials/42-replay-from-ready/). A checkpoint
retains the starting state; it does not add timer or instruction control.

See [determinism](../../determinism.md) for the full boundary.
