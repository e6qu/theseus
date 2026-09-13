# Tutorial 28: Lock Compose VM resources

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`compose.yaml` gives the image two vCPUs and 192 MiB through standard Compose
resource limits. Theseus writes those values into the locked VM plan and
replays the campaign. Use whole vCPU counts and whole MiB memory quantities.
