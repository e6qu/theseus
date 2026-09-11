# Tutorial 14: Run a container image

Run every command from this directory. You need Linux, KVM, and Docker. Set a
published Theseus runtime image:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

Run the tutorial:

```sh
sh ./run.sh
```

`Dockerfile` is an ordinary BusyBox service image. The runner saves it in
Docker's standard image-tar format. The published Theseus runtime runs
`theseus-image flatten`, then `theseus test` and `theseus replay` using the
same generated initramfs.

Replace `Dockerfile` with your service image. Keep its dependencies in the
image. Theseus preserves its entrypoint, environment, and working directory;
no guest SDK or Dockerfile instrumentation is needed.
