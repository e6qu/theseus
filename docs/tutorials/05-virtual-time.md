# Tutorial 5: Virtual time

## Objective

Make a guest's clock advance in fixed steps that you control, instead of
following the host's wall clock. You will boot the same guest twice with
virtual time enabled and see it report the same time within a small
bound, then boot twice with it disabled and see the reported times
diverge. You will also learn exactly where the determinism stops, because
that boundary matters for real workloads.

## The problem

Distributed code makes decisions based on time: timeouts, leader
election, retries, lease expiry. If the guest's clock follows the host's
wall clock, every run sees a different time and those decisions happen at
unrepeatable moments — the same class of flake as random entropy
(tutorial 1), but harder to control.

Theseus's answer is *virtual time*: the guest clock advances by one fixed
*tick* per *quantum*, where a quantum ends after a fixed number of
guest-visible exits (instructions that leave the virtual machine, such as
device accesses). Because exits are deterministic for a given run, the
tick count — and therefore the guest's time — is a function of the seed,
not of the host's clock. The config knob is
`machine-config.virtual_time = { tick_ns, exits_per_tick }`.

## Prerequisites

You need a Linux machine with KVM (the kernel virtual machine feature).
On Apple Silicon macOS, a privileged aarch64 Docker container provides it.
From the repository root, start it like this:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0 bash
```

Inside the container, install the build dependencies and build the
Firecracker fork once:

```sh
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev
cd /theseus/firecracker && cargo build -p firecracker
```

## Steps

### 1. Run the reproducibility test

```sh
cargo test --manifest-path orchestrator/Cargo.toml --lib spawn::tests::test_guest_virtual_time_is_reproducible
```

The test boots a bare-metal guest that reads its own counter (the CPU's
virtual counter, `CNTVCT` on aarch64) and prints it. It boots twice with
virtual time enabled, then twice with it disabled.

You will see it pass with three assertions:

- **Anchored**: with virtual time on, the reported counter is small —
  near zero, because the guest's clock was anchored at virtual time zero
  at boot.
- **Bounded-close**: the two enabled runs report values within a few
  ticks of each other — *not* bitwise identical, and the test explains
  why below.
- **Divergent**: the two disabled runs report different values — that is
  the host wall clock leaking in.

### 2. Understand the boundary

The disabled runs diverge because the guest read its counter between
ticks. Guest counter reads do not cause an exit, so between two tick
boundaries the counter free-runs at host speed — plus whatever host
scheduling jitter delayed the vCPU thread. Measured on real hardware,
this jitter is a few ticks at most.

What this means in practice:

- Code that *waits for events* (waits for packets, completions, signals)
  is fully deterministic: all events arrive at the same tick on replay.
- Code that *busy-reads the clock* and branches on the exact value sees a
  few ticks of noise. Fully closing that gap requires trapping counter
  reads, which the project deliberately does not do today (it would need
  a hypervisor-level change).

This honesty is the point of the tutorial: deterministic *enough* for
event-driven systems, with a documented, measured exception.

## What you have now

A guest clock that advances under your control, an understanding of how
it is configured (`tick_ns`, `exits_per_tick`), and a precise statement
of its determinism boundary.

## Where to go next

[Exploration](../exploration.md) — how ticks interact with branch points
(every branch inherits identical clock state), and
[determinism.md](../determinism.md) — the full list of what is and is not
replayable.
