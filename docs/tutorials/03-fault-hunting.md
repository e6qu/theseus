# Tutorial: Hunting a fault-dependent bug

## Objective

Reproduce a specific data-corruption bug: a replicated counter that
counts one increment **twice** when a network partition drops the
acknowledgement and the sender retries it. You will boot the counter,
watch it behave correctly with no faults, inject the partition between
the command and its acknowledgement, see the duplicate application appear
as a single marker byte, then replay the exact same run and watch the
corruption happen again, byte for byte.

## The problem

Imagine a replicated counter: node A sends increment commands; node B
applies and acknowledges them. Node A retries when no acknowledgement
arrives. The invariant is "every command is applied exactly once."

Now drop the acknowledgement — not the command. A retries; B applies the
retry *again*. One increment counted twice. The bug needs three things at
once: a lost ack, a retry, and an apply that is not idempotent (safe to
repeat). Ordinary tests, which never drop an ack in that exact window,
will never see it.

The tutorial's guest program models exactly this protocol. Its `apply`
is deliberately non-idempotent:

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
`0xEE` ("the ack for the last command was lost") it *retries the last
command* — applying it a second time.

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
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev binutils
rustup target add aarch64-unknown-none
cd /theseus/firecracker && cargo build -p firecracker
```

## Step 1 — Build the counter guest

```sh
sh /theseus/examples/counter-guest/build.sh
```

This compiles the counter program to a flat bootable image
(`counter_guest.bin`) that the hypervisor boots directly, without an
operating system.

## Step 2 — Run the clean schedule

The driver test boots the guest with two increment commands and no
partition, and collects the guest's markers. Run it:

```sh
cd /theseus/orchestrator
cargo test --test counter_tutorial
```

Three assertions run in sequence. The first one checks the clean run:

```
events [0x05, 0x06]  →  markers [0x42, 0x01, 0x01, 0xFF]
```

Read it: boot marker, two first-time applications, round done. The system
is correct when nothing goes wrong.

## Step 3 — Inject the partition

The second assertion feeds `[0x05, 0xEE, 0x06]`: increment 5, partition,
increment 6. The guest behaves exactly as the buggy protocol dictates:
apply 5, lose its ack, *retry* 5, then apply 6. The marker stream
becomes:

```
markers [0x42, 0x01, 0x02, 0x01, 0xFF]
                        ^^
                  the duplicate apply
```

That single `0x02` is the bug, caught in the act. It appears only because
the partition and the retry collided — a combination no hand-written test
would have covered.

## Step 4 — Replay it

The third assertion runs the same partition schedule again. The marker
stream is byte-identical:

```
[0x42, 0x01, 0x02, 0x01, 0xFF]  (again)
```

Because the run is seeded, the replay always reproduces the bug exactly.
A flaky "sometimes it happens" becomes a deterministic "it happens every
time this schedule runs."

## Step 5 — Fix and verify

The fix is idempotency: `apply` should ignore a command it has already
seen. Edit `examples/counter-guest/main.rs` and change the duplicate
branch of `apply` to skip the application (or report a distinct
"deduplicated" marker). Rebuild the guest (`build.sh`) and rerun the
test: the partition schedule produces no `0x02`, and the clean schedule
is unchanged — proving the fix touched only the bug.

## What you have now

- A workload whose correctness is observable as marker bytes.
- A fault schedule that deterministically produces a real bug.
- A replayable proof of that bug, and a way to prove a fix works.

## Further reading

- [determinism.md](../determinism.md) — why seeded runs replay exactly
- [control-channel.md](../control-channel.md) — the protocol the guest
  uses to report markers
- [terminology.md](../terminology.md) — definitions of marker, event,
  and fault schedule
