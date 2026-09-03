# Tutorial 5: Run two connected services

Run every command from this directory. This tutorial creates two tiny Linux
guests, connects them to the `backplane` network, sends `ping` to the API's
UART, and checks that `api` can ping `worker`.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
docker run --rm --privileged -v "$PWD":/tutorial -w /tutorial \
  "$THESEUS_IMAGE" sh ./run.sh
```

`compose.yaml` is a deliberately small Compose file. Each service points to
its own Theseus manifest. Their network settings delay each frame by one
deterministic scheduler round, then add zero or one seeded jitter rounds. That
can deliver a later frame first without using host time. The API announces
serial readiness, then Theseus injects its manifest event directly into that
VM's UART. `theseus compose test`
locks the artifacts, runs the two guests in one deterministic topology, and
leaves their serial logs and results in `theseus-compose-replay/services/`.
`theseus compose replay` runs the locked bundle again and compares every
service serial log, simulated-network topology, and deterministic network
traffic counters and frame-content fingerprints with the original.

`tx_bytes_per_round` limits each service's outbound link. It refills at every
topology round; `0` leaves it unlimited. A frame larger than the budget uses a
full round, so it still makes progress.

`duplicate_ppm = 1000000` duplicates every accepted frame in this example.
The copies use the same simulated link, so bandwidth, delay, and jitter still
apply. Linux networking tolerates these duplicate ARP and ping frames.

Set `corrupt_ppm` to select nonempty frames for one seeded bit flip before the
simulated link delivers them. A duplicate carries the same corrupted bytes.

Each service result includes the first 64 simulated NIC TX, RX, and drop frames.
Each record has a deterministic scheduler round, a drop reason when relevant,
and a hexadecimal payload.

Set `rx_queue_frames` to bound frames waiting for a guest that is not reading
its NIC. Extra frames are recorded as `rx_queue` drops.
