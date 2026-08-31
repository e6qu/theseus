# Terminology

Terms used across Theseus code and documentation. See
[architecture.md](architecture.md) for structure and
[tutorials/](tutorials/) for hands-on runs.

## Core model

- **Deterministic simulation testing** — running a whole system inside an
  environment where every source of nondeterminism (entropy, time,
  scheduling, network faults) is controlled and seed-driven, so any run
  can be replayed bit-for-bit.
- **Seed** — the single input from which all randomness in a run is
  derived. Same seed ⇒ same observable behavior.
- **Timeline** — one execution of the system under test, from a branch
  point (or boot) with a particular seed and fault schedule.
- **Multiverse** — the set of timelines explored in one run; a tree whose
  edges are branch decisions.
- **Branch point** — a captured, complete machine state (serialized state
  + guest RAM in a memfd) from which any number of child timelines can be
  spawned.
- **Child seed** — the seed a branch child diverges with; a deterministic
  function of the branch point's base seed and the branch index
  (`splitmix64(base ^ (index << 32))`).
- **Fault schedule** — the deterministic plan of injected faults per child
  (drop probability in parts per million, partition flag), derived from
  the branch index.

## Replay and verification

- **Replay** — re-executing a timeline from its branch point and seed path;
  must reproduce every observable bit.
- **Seed path** — the chain of seeds from the root to a timeline; the
  replay recipe for it (`TimelineTree::seed_path`).
- **Fingerprint** — per-node proof-of-replay data: entropy probe, marker
  stream, and dirty-page count. Two runs of the same exploration must
  produce identical fingerprints at every node.
- **Entropy probe** — the next bytes the entropy device would serve at
  capture time. Must equal a fresh ChaCha stream of the node's seed.
- **Dirty pages** — count of guest pages written, from the KVM dirty
  bitmap; a memory-footprint coverage signal.
- **Coverage** — the set of guest program counters executed, collected by
  single-stepping (`KVM_GUESTDBG_SINGLESTEP`). The ground-truth signal;
  markers and dirty pages are the cheap proxies.

## Execution machinery

- **Quantum** — a bounded span of guest execution. Quanta are
  **exit-counted**: one ends after `exits_per_tick` guest-visible exits
  (deterministic relative to guest execution), not after host time.
- **Tick** — the fixed amount of virtual time (`tick_ns`) the guest clock
  advances at each quantum boundary.
- **Virtual time (Track B′)** — the guest clock as a pure function of tick
  count: anchored once, then stepped by one tick per quantum (TSC on
  x86_64, CNTVCT offset on aarch64). "B′" marks the deliberate middle
  track between host time and full instruction-level emulation (Track B).
- **Anchor** — the one-time initialization that sets guest-visible time to
  virtual time zero (kvmclock on x86_64, counter offset on aarch64).
- **Track B** — the parked alternative: trapping counter reads for
  instruction-level time fidelity. Deliberately not implemented.
- **Copy-on-write (CoW) branching** — children mapping the branch memfd
  `MAP_PRIVATE`, so sibling timelines share pages until they write them.

## Control channel and guests

- **Control channel ("the door")** — the guest↔host communication device:
  MMIO register file on every platform, plus a serial-console transport
  for Linux guests.
- **Event** — a byte the host pushes into the guest's event FIFO (an input
  or a terminator, `0x00`).
- **Marker** — a byte the guest reports through the log register
  (`0x42` boot, `0xFF` done, anything else application-defined).
- **Command** — a guest lifecycle write to the command register
  (`0x01` = setup complete).
- **Rendezvous** — the protocol round: guest signals setup complete, host
  pushes events plus a terminator, guest echoes/markers them and signals
  done (`0xFF`).
- **Terminator** — event byte `0x00`: ends the current event round.
- **Setup complete** — command byte `0x01`: the workload finished
  initialization and is ready for events.

## Testing vocabulary

- **Property-based testing (PBT)** — stating invariants ("a committed
  write is never lost") and letting the machine search for
  counterexamples, rather than writing example-driven tests (popularized
  for application code by the Hypothesis project).
- **Invariant / property** — an observable statement about the system that
  must hold in every timeline (expressed as markers/assertions in guest
  code).
- **e2e** — the live-KVM proof harness in `e2e/`, which boots real
  microVMs and checks entropy determinism and both control-channel
  transports.
- **Deterministic by construction** — a system built so all
  nondeterminism flows through injectable interfaces (the
  FoundationDB/TigerBeetle discipline); the alternative to hypervisor
  determinism that applies when you own all the code.
