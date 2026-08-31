# Tutorials

Theseus is currently operated through the forked `firecracker` binary and
its Firecracker-compatible HTTP API over a Unix socket. There is not yet a
separate `theseus` command-line program. The first two tutorials therefore
stay at that real host boundary: launch the service, configure it with
`curl`, and observe an ordinary C program in the guest. The third tutorial
adds `theseus-sdk` only when the guest needs to make its own outcomes and
inputs visible.

All runnable tutorials need Linux with KVM. On Apple Silicon, use the
privileged aarch64 Docker container shown in each guide.

## Start here: operate a deterministic service

1. [Run and replay the Theseus service](01-replay-by-seed/) — launch the
   service, boot a plain C guest, and prove that the same `PUT /entropy`
   seed gives the same bytes.
2. [Script a guest through the service API](02-control-the-random/) — send
   a chosen byte script to `PUT /entropy` and watch a plain C guest receive
   exactly those values.
3. [Instrument a guest with `theseus-sdk`](03-the-control-channel/) — add
   lifecycle markers and an event loop when host-side configuration alone is
   no longer enough to judge your program.
4. [Find, replay, and fix a retry bug](04-fault-hunting/) — combine those
   pieces in a small two-node protocol model: establish a healthy baseline,
   inject a lost acknowledgement, replay the failure, then verify a fix.

## Extend the workflow

- [Run your own container image](07-container-images/) — package the
  artifact your CI builds as a bootable guest without adding a guest driver.
- [Explore branch timelines](05-branching-timelines/) — fork a captured
  machine state into isolated futures with distinct seed and fault paths.
- [Control virtual time](06-virtual-time/) — make event-driven timing
  decisions repeatable and understand the precise boundary of that claim.

The deeper design is documented in [architecture.md](../architecture.md),
[determinism.md](../determinism.md), [control-channel.md](../control-channel.md),
and [exploration.md](../exploration.md).
