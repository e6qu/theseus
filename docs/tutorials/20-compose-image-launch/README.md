# Tutorial 20: Change an image launch with Compose

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`worker` is an ordinary BusyBox HTTP image. By default it serves
`/site/default`. `compose.yaml` changes its `working_dir` to
`/site/alternate` and replaces the image command with `.`. The unchanged image
entrypoint therefore serves the alternate file.

`api` fetches `http://worker:8080/identity` as a normal argv operation. The
campaign checks the result and replays the locked topology. The Compose launch
values become part of the worker initramfs before KVM starts; no host command,
shell expansion, or Theseus code runs in either image.
