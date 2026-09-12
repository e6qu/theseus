# Tutorial 21: Configure an image with Compose

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`worker` is a normal BusyBox HTTP image with a default identity file.
`compose.yaml` mounts the local `worker/config/identity` at `/www/identity`.
Theseus locks the file bytes into the worker initramfs before starting KVM.

`api` fetches the file from `worker`, then replays the same campaign. Neither
image reads a host file or contains Theseus code.
