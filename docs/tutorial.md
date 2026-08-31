# Tutorial: hunting a fault-dependent bug with Theseus

A practical, end-to-end run: we take a small replicated workload, drive it
through the Theseus explorer, inject network faults mid-execution, and use
branching + fingerprints to find and replay a bug that no conventional test
would have caught. Every step maps to code that exists and runs today;
where a step currently lives in a test, we say so.

Prerequisites: a Linux+KVM host (a privileged aarch64 Docker container on
Apple Silicon works — see [testing.md](testing.md)) and a built fork
(`cargo build -p firecracker` in `firecracker/`).

## 0. The scenario

Our system: a two-node replicated counter. Node A accepts increment
commands; node B acks them. The invariant we *believe* holds:

> Every accepted increment is acked by exactly one node, in order.

We suspect nothing. The bug: when a network partition lands *between* a
command and its ack, node A retries — and B, which already applied the
first attempt, applies the retry too. One increment, counted twice. A
classic "at-least-once vs exactly-once" fault that only appears under
partition-plus-retry timing. Conventional integration tests never partition
mid-flight, so they never see it.

## 1. Wire the workload to the control channel

Theseus does not read your mind about invariants; you emit **markers** for
the events that matter, and the host injects **events** (inputs) and
**faults**. With the SDK (`sdk/`, bare-metal flavor shown — the Linux
serial transport works identically, see
[control-channel.md](control-channel.md)):

```rust
let door = unsafe { ControlChannel::new(DOOR) };
door.marker(MARKER_BOOT);
door.command(CMD_SETUP_COMPLETE);      // host: I am ready for events

loop {
    door.wait_events();
    let ev = door.pop_event();
    if ev == EVENT_TERMINATOR { door.marker(MARKER_DONE); continue; }

    // The host's event byte is our input: a command to apply.
    let outcome = apply_command(ev);   // may retry internally on timeout!
    door.marker(outcome);              // 0x01 = applied-once, 0x02 = dup!
}
```

Two things to notice:

- `apply_command` contains the retry logic — the bug lives there.
- The workload never sleeps on wall-clock time; retries are driven by
  *events*, so the whole round is replayable (see
  [determinism.md](determinism.md)).

(This is exactly the shape of the bare-metal test guest in
`firecracker/src/vmm/src/test_utils/mock_resources/theseus_guest_rs/main.rs`.)

## 2. Package it

Any Firecracker-bootable artifact works. The tutorial path: an initramfs
with the workload as `/init` (like `e2e/agent/`), booted with:

- entropy seeded (`"seed": 42` — without this nothing is replayable),
- the sim network backend (`PUT /network-interfaces` with
  `"sim": {"seed": 0, "loopback": true, "drop_ppm": 0}`),
- virtual time on for timer-driven retries
  (`machine-config.virtual_time = {tick_ns: 1_000_000, exits_per_tick: 64}`).

## 3. Drive it through the explorer

The explorer boots the root timeline and rendezvous-es through rounds:
push events, wait for done markers, capture branch points. The config
(see `orchestrator/src/orchestrator/explorer.rs`):

```rust
let config = ExplorerConfig {
    events: vec![0x10, 0x11],       // two increments per round
    branch_event_suffix: true,       // each child also gets its index+1
    rendezvous: true,
    faults: Some(FaultStrategy {     // per-child fault schedule
        drop_ppm_base: 0,
        drop_ppm_step: 100_000,      // child k drops 10%·k of frames
        partition_every_n: 3,        // every 3rd child: full partition
    }),
    run_ms: 300,
    branches_per_node: 3,
    max_depth: 1,
};
```

What actually happens per child, deterministically:

1. Child restores from the root branch point (kernel CoW — no RAM copy),
   gets a fresh entropy stream derived from its seed, and the sim-net
   config above.
2. The explorer pushes `0x10, 0x11, child_index+1, 0x00` and waits for the
   done marker.
3. It records the child's fingerprints: entropy probe, marker stream,
   dirty-page count.

## 4. The fingerprints diverge — that's the bug

Marker streams for a healthy run look like:

```
root:    [0x42, 0x01, 0x01, 0x01, 0xFF]   // boot, 3×applied-once, done
child 0: [0x01, 0x01, 0x01, 0x01, 0xFF]
```

But child 2 — the partitioned one, with the partition landing between
command 0x11 and its ack — shows:

```
child 2: [0x01, 0x01, 0x02, 0x01, 0xFF]   // 0x02 = applied twice!
```

The `0x02` marker is the duplicate application. It appears **only** in the
partitioned timeline and **only** when the partition lands in the window
between command and ack — a multi-factorial condition (fault × timing ×
retry policy) that property-based tests on single nodes would never
produce. Because the whole run is seeded, this marker stream is
reproducible bit-for-bit.

(The assertion shape is real: `test_explore_with_reactive_guest` and
`test_explore_with_rust_guest` in the explorer tests check exact marker
streams per child this way.)

## 5. Branch at the bug and replay it exactly

The interesting part is not just seeing the divergence — it is having the
exact timeline. Every node carries:

- a captured `BranchPoint` (full machine state, memfd-backed),
- its seed path (`tree.seed_path(id)` — the replay recipe),
- and its fingerprints.

To replay the buggy timeline exactly:

1. Spawn a child from the root branch point with child 2's seed and fault
   config (`spawn_child` with the same `FaultStrategy` — seeds are
   deterministic functions of `(base_seed, branch_index)`).
2. Re-run the round. The marker stream is identical:
   `[0x01, 0x01, 0x02, 0x01, 0xFF]`.
3. Now debug *inside* the duplicate: pause right before the `0x02` marker,
   capture again, and collect coverage for the round
   (`coverage::collect`) — the PC set shows the retry path executed, and
   comparing it against a healthy child's coverage pinpoints the exact
   branch that retried an already-applied command.

The sibling-isolation guarantee (`test_branch_children_memory_is_cow`)
means we can do destructive analysis on the captured state freely — the
parent and other siblings never see it.

## 6. Fix, and watch the fingerprint go green

The fix (idempotent apply: dedupe by command id) goes into the workload;
re-run the same exploration. Child 2's markers become
`[0x01, 0x01, 0x01, 0x01, 0xFF]` — and because everything is seeded, the
rest of the tree's fingerprints are unchanged, so you know the fix changed
nothing else.

## Where to go next

- Add your own invariants as markers (crashes, monotonicity, ack counts).
- Sweep fault schedules: `drop_ppm` ramps, partitions at different rounds.
- Read [exploration.md](exploration.md) for the tree/parallel machinery,
  [determinism.md](determinism.md) for what is and is not replayable, and
  [control-channel.md](control-channel.md) for the full protocol.
