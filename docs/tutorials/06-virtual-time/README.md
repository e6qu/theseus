# Tutorial: Control the guest clock

## Objective

Make a guest's clock advance in fixed steps you control instead of
following the host's wall clock. You will boot the same guest twice with
virtual time enabled and see it report the same time within a small bound,
then boot twice with it disabled and see the reported times diverge. You
will also learn exactly where the determinism stops — that boundary
matters for real workloads.

## The problem

Distributed code decides things based on time: timeouts, leader elections,
retries, lease expiry. If the guest's clock follows the host's wall clock,
every run sees a different time and those decisions happen at unrepeatable
moments. This is the same flake class as random entropy, but harder to
control.

The engine's answer is *virtual time*: the guest clock advances by one
fixed *tick* per *quantum*, and a quantum ends after a fixed number of
guest-visible exits (moments when the guest's execution leaves the virtual
machine, such as device accesses). Exits are deterministic for a given
run, so the tick count — and therefore the guest's time — is a function of
the seed, not of the host's clock.

## Setup

You need a Linux machine with KVM. On Apple Silicon macOS, a privileged
aarch64 Docker container provides it. From the repository root:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0-bookworm bash
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev
cd /theseus/firecracker && cargo build -p vmm
```

## The configuration

Enable virtual time in the machine config:

```json
"virtual_time": { "tick_ns": 1000000, "exits_per_tick": 64 }
```

`tick_ns` is the virtual nanoseconds the clock advances per quantum;
`exits_per_tick` is the number of guest-visible exits per quantum.

## Run it

A bare-metal guest reads its own counter (`CNTVCT`, the CPU's virtual
counter on aarch64) and prints it. The test boots it twice with virtual
time on, then twice with it off:

```sh
cd /theseus/orchestrator
cargo test --lib spawn::tests::test_guest_virtual_time_is_reproducible
```

The test passes with three assertions:

- **Anchored**: with virtual time on, the reported counter is near zero —
  the guest's clock was anchored at virtual time zero at boot.
- **Bounded-close**: the two enabled runs report values within a few ticks
  of each other — not bitwise identical (see the boundary below).
- **Divergent**: the two disabled runs report different values — the host
  wall clock leaking in.

## Understand the boundary

The enabled runs are not bitwise identical because the guest read its
counter *between* ticks. Guest counter reads do not cause an exit, so
between tick boundaries the counter free-runs at host speed, plus whatever
host scheduling jitter delayed the vCPU thread. Measured on real hardware,
this jitter is a few ticks at most.

In practice:

- Code that *waits for events* (packets, completions, signals) is fully
  deterministic: all events arrive at the same tick on replay.
- Code that *busy-reads the clock* and branches on the exact value sees a
  few ticks of noise. Closing that gap needs counter-read trapping, which
  the engine deliberately does not do today.

## What you have now

A guest clock that advances under your control, the configuration that
drives it (`tick_ns`, `exits_per_tick`), and a precise statement of where
its determinism ends.

## Further reading

- [determinism.md](../../determinism.md) — the full list of what replays and
  what leaks
- [exploration.md](../../exploration.md) — how ticks interact with branch
  points (every branch inherits identical clock state)
- [terminology.md](../../terminology.md) — quantum, tick, anchor, Track B′
