# Tutorial 18: Run a command in a container campaign

Run every command from this directory. You need Linux, KVM, and Docker. Set a
published Theseus runtime image:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`Dockerfile` is an ordinary BusyBox HTTP service. `compose.yaml` declares a
`shell` operation that runs `/bin/cat /www/health` inside that image after it is
ready. Theseus records the exit status, checks its output, checkpoints the
operation, then replays the locked campaign.

Use an argv array with an absolute executable path for `command`; Theseus does
not invoke a shell or search `PATH`. Replace the command with your migration
check, diagnostics command, or integration client.
The image needs no SDK or serial protocol.
