# Tutorial 6: Schedule a service fault

Run every command from this directory. This tutorial pauses, restarts, then
jumps one guest's virtual clock. The schedule is part of `compose.yaml` and is
saved in the replay bundle.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
docker run --rm --privileged -v "$PWD":/tutorial -w /tutorial \
  "$THESEUS_IMAGE" sh ./run.sh
```

`at_round` counts global topology rounds, not host time. A pause resumes after
`duration_rounds`. A restart is a cold boot from that service's locked
artifacts. A `clock_jump` needs `[run.virtual_time]` in that service manifest.

Inspect `theseus-compose-replay/services/service/result.json` for the applied
schedule and the serial log from each boot.
