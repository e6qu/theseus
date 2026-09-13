# Tutorial 24: Gate a service on a Compose health check

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`worker` is a normal BusyBox HTTP image. Its standard Compose `CMD` health
check fetches its own `/health` endpoint. Theseus starts `api` only after that
check passes. `api` then reads `worker`, and replays the same campaign.

The health check is argv, not a shell snippet. Theseus locks its command,
interval, retry count, and start period into the worker initramfs.
