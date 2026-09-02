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
its own Theseus manifest. The API announces serial readiness, then Theseus
injects its manifest event directly into that VM's UART. `theseus compose test`
locks the artifacts, runs the two guests in one deterministic topology, and
leaves their serial logs and results in `theseus-compose-replay/services/`.
