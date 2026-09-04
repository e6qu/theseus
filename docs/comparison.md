# Theseus vs. Antithesis vs. Hypothesis (and related projects)

Where Theseus sits in the landscape of property-based testing, deterministic
simulation, and chaos tooling. Terms are defined in
[terminology.md](terminology.md); a hands-on run is in
[tutorials/](tutorials/).

## The short version

| | Hypothesis (and proptest, QuickCheck) | Antithesis | Theseus |
|---|---|---|---|
| What is tested | Functions / units of code | Whole distributed systems, unmodified | Whole distributed systems, unmodified |
| Method | Generate inputs, check properties | Deterministic hypervisor + guided state-space exploration + fault injection | Same idea, on a Firecracker/KVM fork |
| Replay | Seeded input shrinking | Perfect, instruction-level | Seeded timelines; instruction-exact except mid-quantum clock reads (Track B′ leak, documented) |
| Determinism mechanism | In-process seeded PRNG | Custom deterministic hypervisor (bhyve fork) on bare metal | KVM + seeded devices + tick-stepped virtual clock |
| Fault injection | None | Guided (network, disk, crash, clock) | Sim net drops/partitions + Compose lifecycle/clock candidates |
| Coverage guidance | Shrinking / targeted generators | RL-guided exploration | Marker novelty + dirty pages; deterministic campaign corpus |
| Model | You write properties | You state invariants; product finds bugs | Same |
| License | Open source (MPL) | Commercial (some OSS tools) | AGPL-3.0-or-later (engine); Apache-2.0 (fork) |

## Hypothesis (and the PBT family)

Hypothesis (Python), proptest (Rust), QuickCheck (Haskell) test *code you
own, in-process*: they generate inputs and check that properties hold, then
shrink failing inputs to minimal counterexamples. They are superb for
business logic — parsers, state machines, data structure invariants.

Where they stop: the system under test is one process executing your code.
There is no network to partition, no clock to skew, no node to kill. The
bugs that live in *timing and topology* are out of scope by construction.

Theseus borrows the discipline — **state properties, let the machine
search** — and lifts it to the whole running system.

## Antithesis

Antithesis is the reference product for whole-system deterministic
simulation: a custom deterministic hypervisor (a bhyve fork) runs your
unmodified containers deterministically, an RL-guided explorer injects
faults while searching for new states, and every bug found is perfectly
reproducible with time-travel debugging.

Key architectural differences with Theseus:

- **Determinism boundary**: Antithesis built a hypervisor from scratch so
  it controls the instruction stream itself (instruction-level virtual
  time). Theseus uses KVM and gains time control at tick granularity —
  exit-counted quanta with TSC/CNTVCT stepped at boundaries. The tradeoff:
  guest counter reads between boundaries free-run at host rate (a
  documented, measured leak); Antithesis does not have it.
- **Cost and complexity**: Antithesis needs bare-metal hypervisor
  engineering (custom time sources, CPU quirk taming). Theseus rides KVM's
  commodity path — far less mechanism, no kernel patches, at the price of
  that leak.
- **Exploration guidance**: Antithesis uses coverage-guided RL at scale.
  Theseus currently retains Compose campaign timelines with new application
  markers, while its single-VM explorer uses deterministic DFS and marker
  novelty ordering. Ground-truth single-step coverage exists as the reference
  for a future fast instrumentor.
- **Campaign interface**: Antithesis can drive unmodified workloads through
  its test templates and property APIs. Theseus accepts a designated Compose
  driver, text UART operations, lifecycle/clock candidates, barrier-triggered
  named-network `partition`/`heal`, directed service-to-service
  `link_partition`/`link_heal`, simulated-drive `storage_fault`, and
  `always`/`sometimes`/`reachable`/`unreachable` serial properties. It reduces
  an individual violation by re-running locked full topologies, supports
  bounded fault sequences, and fingerprints the applied actions on replay, but
  it does not yet have Antithesis's
  copy-on-write whole-topology snapshots or its large-scale RL scheduler.

If you can pay for the product and want instruction-exact replay plus
vendor support, use Antithesis. Theseus exists as an open,
KVM-native, hackable engine in the same intellectual family.

## FoundationDB

FoundationDB is where deterministic simulation testing was proven at
scale. The database is written in Flow, a C++ actor-model extension, and
every source of nondeterminism — network, disk, clock, timers,
randomness — flows through abstract interfaces. In simulation mode those
interfaces are backed by a deterministic discrete-event simulator with a
seeded generator: the whole cluster runs on one thread, while the
simulator kills machines, partitions networks, corrupts disks, and skews
clocks. Nightly, millions of seeded runs torture the system; every
failure replays exactly from its seed.

Key characteristics:

- **Deterministic by construction.** The deepest possible control — the
  code itself only ever sees the simulated environment — at the price of
  writing the entire system against Flow. Nothing else may run in that
  process.
- **Faster than real time.** One thread simulating a cluster outruns any
  deployment of real machines by orders of magnitude.
- **Total replay.** A failing run is a seed; the seed is the bug report.

Theseus trades that depth for breadth: instead of rewriting the system
against a framework, the hypervisor forces determinism on unmodified
binaries. The price on our side is real: mid-quantum clock reads free-run
at host rate (documented in [determinism.md](determinism.md)), and a VM
boundary costs more than an in-process event loop. The price on their
side: years of framework discipline before the first test, and nothing
outside the framework can be tested at all.

(Antithesis, for the record, was founded by FoundationDB veterans — the
simulator's methodology generalized into a product. Theseus sits in the
same family tree.)

## TigerBeetle's VOPR

TigerBeetle is a financial-transactions database written in Zig, tested
by VOPR (the "Viewstamped Operation Replicator" fuzzer): a deterministic
simulator that runs the whole cluster in one process against a simulated
network, storage, and clock. VOPR's notable ideas, beyond plain schedule
fuzzing:

- a **state checker** that validates the simulated cluster against a
  model after every run — the replica under test is compared to a
  reference, not just probed for crashes;
- **fault injection in the storage layer** (torn writes, misreads, corrupt
  sectors), not just the network;
- **performance fuzzing** — the simulator tracks operation latencies and
  flags regressions, catching slowdowns the way correctness fuzzing
  catches crashes.

VOPR is the strongest case for the deterministic-by-construction school
done in a modern systems language: single-threaded, seeded, thousands of
runs per second, and it found real consensus bugs in TigerBeetle's
Viewstamped Replication implementation before anyone ran them in
production. Like FoundationDB, it can only test code written inside its
simulated runtime. Theseus's VM boundary is slower per run and needs a
KVM host, but the system under test needs no simulator port.

## Other related projects

- **madsim / turmoil (Rust)** — deterministic simulation of tokio-based
  systems via drop-in runtime shims. Same "rewrite against a simulated
  runtime" trade as FoundationDB and TigerBeetle: lighter than a
  hypervisor, but only for code built on those runtimes.
- **Jepsen** — the gold standard for *analyzing* distributed-system
  histories for consistency violations on real infrastructure. Jepsen
  runs real nodes on real networks; bugs are real but reproduction is
  flaky and there is no replay. Theseus trades some realism for perfect
  replay.
- **Chaos engineering (Chaos Monkey, Litmus, Gremlin)** — randomized fault
  injection on live systems. Finds real problems, but blindly (no
  state-space guidance) and irreproducibly (no determinism).
- **Formal methods (TLA+, P)** — prove properties of *models* of systems.
  Complementary: proofs about designs, Theseus explores the actual
  implementation, binary included.

## When to use what

- Unit/property bugs in pure logic → Hypothesis/proptest.
- You own all the code and can re-architect → deterministic-by-construction
  (FoundationDB-style, madsim, TigerBeetle).
- You need instruction-exact replay of unmodified systems with vendor
  support →
  Antithesis.
- You want an open, KVM-based engine you can read, modify, and embed →
  Theseus.
