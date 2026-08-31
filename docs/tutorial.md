# Tutorials

A series of hands-on tutorials, ordered by increasing complexity. Each one
is fully self-contained: it states its objective, explains the specific
problem it solves, gives you every command to run, and shows the output you
should see. Terms you meet for the first time are defined where they appear
and in [terminology.md](terminology.md).

All tutorials need a Linux+KVM host. On Apple Silicon, a privileged
aarch64 Docker container provides `/dev/kvm`; see
[testing.md](testing.md) for the exact command.

1. [Replay by seed](tutorials/01-replay-by-seed.md) — prove that one seed
   makes two virtual machine boots byte-identical, and why that matters.
2. [The control channel](tutorials/02-the-control-channel.md) — make a
   guest and a host talk to each other through Theseus's device channel.
3. [Hunting a fault-dependent bug](tutorials/03-fault-hunting.md) — catch
   a duplicate-apply bug that only appears when a partition and a retry
   collide, then replay it bit-for-bit.
4. [Branching timelines](tutorials/04-branching-timelines.md) — pause a
   running system, fork it into children that diverge only by seed and
   fault schedule, and prove their isolation.
5. [Virtual time](tutorials/05-virtual-time.md) — take control of the
   guest's clock and learn exactly how deterministic the result is.
6. [Run a container image as a virtual machine](tutorials/06-container-images.md)
   — boot the artifact your CI builds, with the control channel wired in
   and no changes to the image.

Related reading: [architecture.md](architecture.md),
[determinism.md](determinism.md), [comparison.md](comparison.md).
