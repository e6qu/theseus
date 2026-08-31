# Tutorial: Fork the machine mid-run

## Objective

Pause a running virtual machine, capture its complete state in memory, and
fork two child timelines from it. You will verify that each child's
entropy stream is a fresh stream of the child's own seed (not the parent's
continuation), that a write through one child's memory is invisible to its
sibling and to the branch point, and that children spawned with different
fault schedules each carry their assigned network faults. When you finish,
you will have the machinery that turns one run into many.

## The problem

Exploring failures one run at a time is slow, and the interesting moment —
the exact interleaving where a bug might live — may never reappear if you
restart from boot. The answer is a *branch point*: a complete snapshot of
the machine held in memory (state plus a full guest-RAM dump in a `memfd`,
an in-memory file), from which any number of child timelines resume
independently. Two terms: a *timeline* is one execution of the system from
a branch point with a particular seed and fault schedule; *copy-on-write*
means children share the branch point's memory pages until they write
them, so forking is cheap and siblings cannot observe each other.

## Setup

You need a Linux machine with KVM. On Apple Silicon macOS, a privileged
aarch64 Docker container provides it. From the repository root:

```sh
docker run --rm -it --platform linux/arm64 --privileged \
  -v "$PWD":/theseus -w /theseus rust:1.97.0 bash
apt-get update -qq && apt-get install -y -qq libclang-dev libseccomp-dev
cd /theseus/firecracker && cargo build -p vmm
```

## Step 1 — Two children, diverging only by seed

```sh
cd /theseus/orchestrator
cargo test --lib spawn::tests::test_branch_children_diverge_only_by_seed
```

The test boots a virtual machine with a seeded entropy device, pauses it,
captures a branch point, and spawns two children. It asserts:

- the two children's entropy streams differ from each other, and
- each child's stream equals a fresh ChaCha stream of its own seed — not a
  continuation of the parent's.

Children are identical in every way except the seed. Everything else —
memory, registers, devices, clock state — is shared from the branch point.

## Step 2 — Prove sibling isolation

```sh
cargo test --lib spawn::tests::test_branch_children_memory_is_cow
```

The test writes a marker through child A's guest memory and asserts that
child B and the branch point itself still hold the old contents. Children
map the branch point's memory file with `MAP_PRIVATE`: the kernel copies a
page only when a child writes it. Siblings are truly independent, and
forking costs almost nothing until children start writing.

## Step 3 — Give each child a different fate

```sh
cargo test --lib spawn::tests::test_spawn_child_with_fault_schedule
```

One parent, three children: the first gets no packet loss, the second a
10% drop probability, the third a full network partition. The test reads
each child's restored network configuration back and asserts the schedule
was applied. That is the second axis of divergence: same branch point,
same seed lineage, different faults.

## What you have now

- A way to freeze any moment and run many futures from it.
- Proof that child timelines diverge only by seed and fault schedule, and
  are memory-isolated from each other.
- The machinery behind the parallel explorer.

## Further reading

- [exploration.md](../exploration.md) — the timeline tree and the explorer
- [determinism.md](../determinism.md) — the replay fingerprints each node
  records
- [terminology.md](../terminology.md) — branch point, multiverse,
  copy-on-write
