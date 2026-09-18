# Tutorials

Each directory is complete: make it your working directory and use the
published Theseus artifact named in its README. No tutorial needs a Theseus
source checkout.

Tutorials 1–8, 10–11, and 14–41 need Linux with KVM and Docker. Tutorials 9,
12, and 13 read recorded bundles with only the published binary. Tutorials 1
and 2 currently use the arm64 runtime, because that is where the matching
deterministic-CRNG kernel module is shipped.

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
9. [Inspect a recorded exploration](09-inspect-report/) — write browser,
   issue, and CI reports without needing KVM.
10. [Find a bad topology timeline](10-autonomous-compose-campaign/) — drive a
    three-service Compose campaign through UART operations and report an
    intentional runtime-property failure.
11. [Certify a deterministic runtime](11-certify-runtime/) — run a fixed KVM
    topology from one retained ready checkpoint twice and inspect its witness.
12. [Investigate two campaign bundles](12-investigate-campaign/) — find the
    first recorded divergence and query retained evidence without a VM.
13. [Read a public evaluation](13-public-evaluation/) — distinguish a locked
    format fixture from independently replayable runtime evidence.
14. [Run a container image](14-container-image/) — convert, test, and replay
    an unmodified service image with the published Linux runtime.
15. [Campaign an unmodified container service](15-container-campaign/) — drive
    a declared HTTP operation, retain its checkpoint, and replay it.
16. [Publish a public campaign](16-public-campaign/) — copy a completed
    container campaign into a locked, offline-readable evaluation.
17. [Campaign a gRPC health service](17-grpc-campaign/) — drive the standard
    gRPC health endpoint of an unmodified container service.
18. [Run a container command](18-container-command/) — run an argv command in
    an unmodified service image and replay its checked result.
19. [Connect container services by name](19-compose-image-network/) — use a
    deterministic Compose network from unmodified service images.
20. [Change an image launch with Compose](20-compose-image-launch/) — replace
    an image command and working directory without changing the image.
21. [Configure an image with Compose](21-compose-config/) — lock a local
    read-only config file into an unmodified service image.
22. [Supply an image secret with Compose](22-compose-secret/) — lock a local
    root-only secret file into an unmodified service image.
23. [Seed image data with a Compose bind volume](23-compose-volume/) — lock a
    local writable data directory into an unmodified service image.
24. [Gate a service on a Compose health check](24-compose-healthcheck/) — use
    a standard argv health check before starting a dependent image service.
25. [Load image environment from a Compose file](25-compose-env-file/) — lock
    literal `env_file` values into an image service and replay them.
26. [Set Compose host identity](26-compose-host-identity/) — lock a hostname
    and local `extra_hosts` aliases into an image service and replay them.
27. [Run an image as a Compose user](27-compose-user/) — lock numeric image
    credentials into a service and replay them.
28. [Lock Compose VM resources](28-compose-resources/) — turn standard Compose
    CPU and memory limits into a replayed VM contract.
29. [Overlap ordinary commands](29-overlap-commands/) — launch named processes,
    find a lost update, minimize it, and replay its completion observations.
30. [Reproduce a lost update across services](30-multiservice-lost-update/) —
    verify a partition and recovery, then overlap two ordinary worker images
    while one example exercises the complete locked Compose runtime contract.
31. [Guide a campaign with C basic-block coverage](31-c-basic-block-coverage/) —
    compile an ordinary C command with the published runtime, retain stable
    application-block identities, and verify them on replay.
32. [Reproduce a C thread race](32-deterministic-thread-scheduling/) — control
    application basic-block interleavings, retain every runnable-set choice,
    and replay a lost update without the guest SDK.
33. [Search C thread schedules](33-search-thread-schedules/) — enumerate a
    bounded set of pthread interleavings, find a lost update, minimize it, and
    replay its exact schedule without the guest SDK.
34. [Explore runnable thread choices](34-explore-runnable-prefixes/) — grow a
    bounded schedule tree from observed runnable sets and replay the failing
    choice prefix without the guest SDK.
35. [Control pthread synchronization](35-control-pthread-synchronization/) —
    schedule mutex and condition-variable waits, retain their stable events,
    and replay them without the guest SDK.
36. [Explore structured choices](36-structured-choices/) — give a plain C
    command bounded choices, find a failing combination with unified guidance,
    and replay the exact decision trace without the guest SDK.
37. [Guide a campaign with LLVM edge coverage](37-llvm-edge-coverage/) —
    instrument C++ and a dynamically loaded native module with the published
    LLVM tools, lock their symbols, report reached source lines, and replay the
    build-scoped edges.
38. [Cover a Cargo workspace](38-cargo-workspace-coverage/) — instrument a
    Rust command and its static Rust workspace dependencies in one published-tool
    build, then retain source-associated edges through reporting and replay.
39. [Cover a Go module](39-go-module-coverage/) — instrument a Go command and
    its imported main-module packages in one published-tool build, then retain
    source-associated blocks through reporting and replay.
40. [Explore test templates with automatic faults](40-compose-test-commands/) —
    package two command sets, derive service and asymmetric link faults from a
    two-service topology, then minimize and replay a lost update.
41. [Inspect low-level execution](41-reject-execution-divergence/) — run an
    uninstrumented container, retain its per-vCPU and machine-wide execution
    streams, and compare those observations with host-input replay.

42. [Replay from a ready checkpoint](42-replay-from-ready/) — boot an RNG-backed
    guest once, retain its state, and replay serial and standard random-device
    reads from that same checkpoint without claiming deterministic kernel boot.

For source-tree work, see the [fault-hunting exercise](../developer/fault-hunting/)
and the focused guides for [container images](../guides/container-images/),
[branching](../guides/branching-timelines/), and
[virtual time](../guides/virtual-time/).

For design details, see [determinism](../determinism.md),
[the control channel](../control-channel.md), and [exploration](../exploration.md).
