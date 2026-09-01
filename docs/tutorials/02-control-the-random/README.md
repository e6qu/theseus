# Tutorial 2: Choose the random input

Run every command from this directory. Set the published runtime image as in
tutorial 1, then choose a seed:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
export SEED=42
docker run --rm --privileged --platform linux/arm64 \
  -e SEED -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh ./run.sh
```

The guest reads one `u32` from `/dev/urandom`. Run the same command again:
the value is the same. Change `SEED`, then run it again: the value changes.

Theseus chooses the seed, not individual bytes from Linux's CSPRNG. That
keeps ordinary programs working unchanged: they keep reading
`/dev/urandom` or `/dev/random`.
