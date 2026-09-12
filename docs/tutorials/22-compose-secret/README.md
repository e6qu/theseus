# Tutorial 22: Supply an image secret with Compose

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`worker` is a normal BusyBox HTTP image. `compose.yaml` replaces its default
`/www/token` with `worker/secret/token`. Theseus locks those bytes as a
root-only file in the worker initramfs. `api` reads the token, then replays the
same campaign. Treat the campaign and replay directories as sensitive: they
contain the locked secret.
