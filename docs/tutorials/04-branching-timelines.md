# Tutorial 4: Branching timelines

## Objective

Pause a running virtual machine, capture its complete state in memory,
and fork two child timelines from it. You will verify three things: each
child's entropy stream matches a fresh stream seeded from the child's own
seed (not the parent's continuation); a write through one child's memory
is invisible to the other child and to the branch point; and three
children spawned with different fault schedules (no loss, 10% loss, full
partition) each carry their assigned configuration.

## The problem

One run at a time is slow exploration. To search for bugs you want to ask
"many what-ifs" from the same interesting moment: what if the network
partitions *here*? What if a node dies *then*? Re-running from the
beginning each time wastes the setup work and risks the moment never
reappearing. The answer is a *branch point*: a complete snapshot of the
machine — every byte of memory, every register, every device state — held
in memory, from which any number of child timelines can resume
independently.

Key terms: a *timeline* is one execution of the system from a branch
point with a particular seed and fault schedule. The *multiverse* is the
tree of timelines explored in one run. *Copy-on-write* means children
share the parent's memory pages until they write to them, so forking is
cheap and siblings cannot observe each other.

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

### 1. Watch two children diverge by seed alone

Run the branch test:

```sh
cargo test --manifest-path orchestrator/Cargo.toml --lib spawn::tests::test_branch_children_diverge_only_by_seed
```

The test boots a virtual machine with a seeded entropy device, pauses it,
captures a branch point (the serialized machine state plus a full dump of
guest memory into a `memfd` — an in-memory file), and spawns two
children. Each child gets a new seed derived deterministically from the
branch point. The test asserts:

- the two children's entropy streams differ from each other, and
- each child's stream equals a fresh ChaCha stream of its own seed — not
  a continuation of the parent's.

That is the core of branching: children are identical in every way
*except* the seed. Everything else — memory, registers, devices, clock
state — is shared.

### 2. Prove siblings cannot see each other

Run the isolation test:

```sh
cargo test --manifest-path orchestrator/Cargo.toml --lib spawn::tests::test_branch_children_memory_is_cow
```

It writes a marker string through child A's guest memory and asserts that
child B and the branch point itself still hold the old contents. The
children map the branch point's memory file with `MAP_PRIVATE`, so the
kernel copies a page only when a child writes it. Siblings are truly
independent timelines, and forking costs almost nothing until the
children start writing.

### 3. Give each child a different fault schedule

Run the fault-schedule test:

```sh
cargo test --manifest-path orchestrator/Cargo.toml --lib spawn::tests::test_spawn_child_with_fault_schedule
```

One parent, three children: the first gets no packet loss, the second a
10% drop probability, the third a full network partition. The test reads
each child's restored network configuration back and asserts the schedule
was applied. This is the second axis of divergence: same branch point,
same parent seed lineage, different fates.

## What you have now

- A way to freeze any interesting moment and replay many futures from it.
- Proof that child timelines diverge only by seed and fault schedule, and
  are isolated from each other.
- The machinery behind the explorer's parallel exploration from tutorial 3
  and tutorial 5.

## Next

[Virtual time](05-virtual-time.md) — make the guest's clock as
deterministic as its entropy.
