# Tutorials

Each directory is complete: make it
your working directory and use the published Theseus image named in its
README. No tutorial needs a Theseus source checkout.

All runners need Linux with KVM and Docker. Tutorials 1 and 2 currently use
the arm64 runtime, because that is where the matching deterministic-CRNG
kernel module is shipped.

1. [Replay `/dev/urandom`](01-replay-by-seed/) — replay ordinary Linux random
   devices with a seed.
2. [Choose the random input](02-control-the-random/) — select a new random
   stream without changing the guest.
3. [Instrument a guest](03-the-control-channel/) — use the published
   `theseus-sdk` package for markers and events.
4. [Read a serial device](04-read-serial/) — feed a UART/TTY value into a
   guest, as you would on a Raspberry Pi.
5. [Run two connected services](05-compose-topology/) — connect two guests
   through a deterministic Compose backplane.
6. [Schedule a service fault](06-lifecycle-clock/) — pause, restart, and jump
   one service's virtual clock at deterministic topology rounds.
7. [Inject storage faults](07-storage-faults/) — exercise a deterministic,
   memory-only virtio disk.
8. [Explore an SDK guest](08-explore-sdk-guest/) — branch a control-channel
   guest within a fixed timeline budget.

For source-tree work, see the [fault-hunting exercise](../developer/fault-hunting/)
and the focused guides for [container images](../guides/container-images/),
[branching](../guides/branching-timelines/), and
[virtual time](../guides/virtual-time/).

For design details, see [determinism](../determinism.md),
[the control channel](../control-channel.md), and [exploration](../exploration.md).
