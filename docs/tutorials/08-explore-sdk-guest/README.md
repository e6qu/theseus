# Tutorial 8: Explore an SDK guest

Run every command from this directory. Build the small aarch64 guest first.
It downloads `theseus-sdk` from the published release; it does not use a
Theseus checkout.

```sh
export THESEUS_TAG=<12-character-sha>
rustup target add aarch64-unknown-none
sh ./build.sh
```

Then run the published arm64 runtime on a Linux/KVM arm64 host:

```sh
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:${THESEUS_TAG}-arm64
docker run --rm --privileged -v "$PWD":/tutorial -w /tutorial \
  "$THESEUS_IMAGE" sh ./run.sh
```

The guest signals setup, reads `A` from its UART, receives `90` through the
SDK control channel, and signals completion. Theseus injects the UART byte
into each timeline directly; it never uses shared host stdin. It forks up to
seven timelines and checks that each one read the byte and completed. It
records every seed path and check outcome in `theseus-exploration/result.json`.
The bundle also contains the exact `theseus-explorer` binary used for replay.

Replay those locked artifacts without the original manifest:

```sh
theseus explore --replay theseus-exploration --output exploration-replay
```
