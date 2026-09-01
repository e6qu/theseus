# Tutorial 1: Replay `/dev/urandom`

Run every command from this directory. You need Linux, KVM, Docker, and a
published Theseus runtime image. Choose a published short commit SHA and set
it once:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
```

Run the tutorial:

```sh
docker run --rm --privileged --platform linux/arm64 \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh ./run.sh
```

The guest is only [`init`](init). It loads the matching seed loader, then
dumps 16 bytes from `/dev/urandom` and `/dev/random` with `od`.

The runner boots the guest with seeds `42`, `42`, and `1337`. Equal seeds
produce equal output; the third run differs.

Keep the seed with a failure. It is the replay input.
