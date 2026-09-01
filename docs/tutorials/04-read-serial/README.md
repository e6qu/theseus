# Tutorial 4: Read a serial device

Run every command from this directory. Set the published runtime image, then
send the value that a sensor would have produced:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
export READING=21.5C
docker run --rm --privileged -e READING \
  -v "$PWD":/tutorial -w /tutorial "$THESEUS_IMAGE" sh ./run.sh
```

The guest opens `/dev/ttyS0`, reads one line, and prints it back. The runner
writes `READING` to Firecracker's standard input and captures the serial
output through `PUT /serial`.

On a Raspberry Pi, the UART is also a TTY. Its device is commonly
`/dev/ttyAMA0` or `/dev/ttyS0`; use the one selected by the board's serial
configuration. The interaction is the same as this example.
