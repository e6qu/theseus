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

## Hold kernel timers and observe their arming

Under virtual time, in-kernel timer deliveries are invisible to the
machine stream. Two `run.virtual_time` flags close part of that gap on
amd64:

- `hold_kernel_timers` holds an asserted LAPIC-timer delivery at the
  handled exit where it appears and injects it at its recorded stream
  turn, which replay gates like any other vCPU turn.
- `filter_timer_msrs` (stage 1 of deterministic arming) steers the guest
  into x2APIC mode and routes the timer registers (TMICT, TMCCT, TDCR,
  and the TSC-deadline MSR) to userspace, so every arming write is
  logged with its virtual timestamp. In hold mode the arming write is
  also a recorded machine-stream decision, and the delivery admits only
  against its own arm.

The arm→delivery pairing (the delivery admits only against its own
arm) and the deterministic-arming design are in
[design/deterministic-timer-arming.md](../../design/deterministic-timer-arming.md).

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
