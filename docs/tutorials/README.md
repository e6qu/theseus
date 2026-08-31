# Tutorials

Start with the first four tutorials in order. They use Theseus at its current
host surface: the forked `firecracker` binary and its Unix-socket HTTP API.
Tutorials 1 and 2 use plain C guests. Tutorial 3 introduces `theseus-sdk`
only when the guest must report an outcome. Tutorial 4 uses those pieces to
find and replay a bug.

All commands need Linux with KVM. On Apple Silicon, run them in the privileged
aarch64 container from tutorial 1.

1. [Run and replay a VM](01-replay-by-seed/) — seed the service and replay a
   plain C guest.
2. [Script guest randomness](02-control-the-random/) — choose the bytes a
   plain C guest receives.
3. [Instrument a guest](03-the-control-channel/) — use `theseus-sdk` for
   events and markers.
4. [Find and replay a retry bug](04-fault-hunting/) — establish a baseline,
   inject a fault, reproduce it, and verify a fix.

Afterward, use the focused guides for [container images](07-container-images/),
[branching](05-branching-timelines/), and [virtual time](06-virtual-time/).

For design details, see [determinism](../determinism.md),
[the control channel](../control-channel.md), and [exploration](../exploration.md).
