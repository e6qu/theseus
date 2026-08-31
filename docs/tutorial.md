# Tutorials

A series of hands-on tutorials, ordered by increasing complexity. Each one
stands alone: it states its objective, explains the specific problem it
solves, gives you every command to run, and shows the output you should
see. Terms are defined where they appear and in
[terminology.md](terminology.md).

All tutorials need a Linux+KVM host. On Apple Silicon, a privileged
aarch64 Docker container provides `/dev/kvm`; each tutorial gives the exact
command.

1. [Replay a failing test by seed](tutorials/01-replay-by-seed.md) — prove
   that one seed makes two virtual machine boots byte-identical.
2. [Control the random](tutorials/02-control-the-random.md) — force the
   guest's random number generator to return values you chose.
3. [Give the guest a voice](tutorials/03-the-control-channel.md) — make a
   guest report results and accept commands through Theseus's device
   channel.
4. [Catch a partition-and-retry corruption](tutorials/04-fault-hunting.md) —
   catch a duplicate-apply bug that only appears when a partition and a
   retry collide, then replay it bit-for-bit.
5. [Fork the machine mid-run](tutorials/05-branching-timelines.md) — pause
   a running system, fork it into children that diverge only by seed and
   fault schedule, and prove their isolation.
6. [Control the guest clock](tutorials/06-virtual-time.md) — take control
   of the guest's clock and learn exactly how deterministic the result is.
7. [Run a container image as a virtual machine](tutorials/07-container-images.md)
   — boot the artifact your CI builds, with the control channel wired in
   and no changes to the image.

Related reading: [architecture.md](architecture.md),
[determinism.md](determinism.md), [comparison.md](comparison.md).
