# Tutorial: Catch a partition-and-retry corruption

## Objective

Reproduce a specific data-corruption bug: a replicated counter that
counts one increment **twice** when a network partition drops the
acknowledgement and the sender retries it. You will watch the counter
behave correctly with no faults, inject the partition between command and
acknowledgement, see the duplicate application appear as one marker byte,
then replay the run and watch the corruption happen again, byte for byte.

## The problem

Node A sends increment commands; node B applies and acknowledges them.
Node A retries when no acknowledgement arrives. The invariant is "every
command is applied exactly once."

Drop the acknowledgement — not the command. A retries; B applies the
retry *again*. One increment counted twice. The bug needs a lost ack, a
retry, and a non-idempotent apply all at once. Ordinary tests, which
never drop an ack in that window, will never see it.

The workload models exactly this protocol. Its `apply` is deliberately
non-idempotent:

```rust
fn apply(applied: &mut u32, id: u8, door: &ControlChannel) {
    let bit = 1u32 << (id & 31);
    if *applied & bit != 0 {
        door.marker(0x02);          // duplicate application (the bug)
    } else {
        *applied |= bit;
        door.marker(0x01);          // applied once
    }
}
```

Its event loop applies each command event, and on the partition event
`0xEE` ("the ack for the last command was lost") it retries the last
command — applying it a second time.

## Setup

You need a Linux machine with KVM. On Apple Silicon macOS, a privileged
aarch64 Docker container provides it. From the repository root:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0 bash
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev binutils
rustup target add aarch64-unknown-none
cd /theseus/firecracker && cargo build -p firecracker
```

## Step 1 — Build the workload

```sh
sh /theseus/examples/counter-guest/build.sh
```

This compiles the counter to a flat bootable image that the hypervisor
boots directly, without an operating system.

## Step 2 — Run the clean schedule

The driver boots the guest with two increments and no partition. Run the
test:

```sh
cd /theseus/orchestrator && cargo test --test counter_tutorial
```

Three assertions run. The first checks the clean run:

```
events [0x05, 0x06]  →  markers [0x42, 0x01, 0x01, 0xFF]
```

Boot marker, two first-time applications, round done. The system is
correct when nothing goes wrong.

## Step 3 — Inject the partition

The second assertion feeds `[0x05, 0xEE, 0x06]`: increment 5, partition,
increment 6. The guest applies 5, loses its ack, retries 5, applies 6:

```
markers [0x42, 0x01, 0x02, 0x01, 0xFF]
                        ^^
                  the duplicate apply
```

That single `0x02` is the bug, caught in the act — present only because
the partition and the retry collided.

## Step 4 — Replay it

The third assertion runs the same schedule again. The marker stream is
byte-identical:

```
[0x42, 0x01, 0x02, 0x01, 0xFF]  (again)
```

The run is seeded, so the replay always reproduces the bug exactly. A
flaky "sometimes" becomes a deterministic "every time this schedule runs."

## Step 5 — Fix and verify

Make `apply` idempotent: ignore a command already seen. Edit
`examples/counter-guest/main.rs` and change the duplicate branch of
`apply` to skip the application (or report a distinct "deduplicated"
marker). Rebuild the guest and rerun the test: the partition schedule
produces no `0x02`, and the clean schedule is unchanged — the fix touched
only the bug.

## What you have now

- A workload whose correctness is observable as marker bytes.
- A fault schedule that deterministically produces a real corruption.
- A replayable proof of the bug, and a way to prove a fix works.

## Further reading

- [determinism.md](../determinism.md) — why seeded runs replay exactly
- [control-channel.md](../control-channel.md) — the protocol the guest
  reports markers over
- [terminology.md](../terminology.md) — marker, event, fault schedule
