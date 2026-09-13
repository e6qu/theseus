# Tutorial 27: Run an image as a Compose user

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`api` is a normal BusyBox HTTP image. `compose.yaml` starts it as numeric user
`1000:1000`. Theseus checks the effective UID inside the guest, then replays
the campaign.

Use a numeric `uid:gid`. Theseus locks the credentials into the image runtime;
it does not consult host or image account-name lookup.
