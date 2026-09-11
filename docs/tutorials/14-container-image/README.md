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

`Dockerfile` is an ordinary BusyBox HTTP service. `theseus.toml` names its
Docker archive as `guest.image`, then declares a readiness endpoint and an
HTTP assertion. Theseus starts the service, waits for readiness, checks
`/health`, records the result, and stops the service.

Replace `Dockerfile` with your own image. Change the URLs and assertions in
`theseus.toml` to match it. Keep the service dependencies in the image; no
guest SDK or Dockerfile instrumentation is needed.
