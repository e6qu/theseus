# Tutorial 23: Seed image data with a Compose bind volume

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`worker` is a normal BusyBox HTTP image. `compose.yaml` replaces its `/www`
directory with `worker/site`. Theseus copies that directory into the worker
initramfs before starting KVM. `api` reads the seeded file, then replays the
same campaign.

The directory is writable while the VM runs, but every replay starts from the
same locked bytes. Theseus supports local Compose bind sources here, not
Docker named or shared volumes.
