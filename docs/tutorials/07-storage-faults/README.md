# Tutorial 7: Inject storage faults

Run every command from this directory. This guest writes one sector to an
in-memory `/dev/vda`, reads it back, and observes the configured corruption.
The disk never names or touches a host file.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
docker run --rm --privileged -v "$PWD":/tutorial -w /tutorial \
  "$THESEUS_IMAGE" sh ./run.sh
```

Change the `[[storage]]` values in `service/theseus.toml`, then run the same
command again. `latency_rounds` advances when the topology runner pumps the
guest; it never sleeps on host time. The locked replay plan records the disk
settings and its derived seed. A replay also compares the final bytes of every
simulated drive with the original bundle.
