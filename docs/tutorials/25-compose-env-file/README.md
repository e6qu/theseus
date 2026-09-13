# Tutorial 25: Load image environment from a Compose file

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`api` is a normal BusyBox HTTP image. `compose.yaml` loads literal values from
`api/service.env`. Theseus locks those values before the campaign starts, runs
`env` in the image to show them, then replays the same campaign.

`env_file` never reads the host environment during a run. Use literal
`KEY=value` lines; Theseus rejects bare names and `${...}` interpolation.
