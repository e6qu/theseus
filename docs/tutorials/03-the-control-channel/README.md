# Tutorial 3: Mark guest state with `theseus-sdk`

This directory is a complete bare-metal guest. It gets `theseus-sdk` from the
published GitHub release, not from a Theseus checkout.

Choose a published short commit SHA, then build it:

```sh
export THESEUS_TAG=<12-character-sha>
rustup target add aarch64-unknown-none
sh ./build.sh
```

`build.sh` downloads the SDK package into `vendor/`, builds the guest, and
writes `guest.bin` in this directory.

The guest uses `ControlChannel` to do three things:

1. Detect the Theseus control device.
2. Emit `MARKER_BOOT` and `CMD_SETUP_COMPLETE`.
3. Echo events as markers, then emit `MARKER_DONE`.

Markers are the small, stable contract between a guest and the Theseus
service. Keep values meaningful: for example, use one marker for “ready” and
one for a failed invariant.
