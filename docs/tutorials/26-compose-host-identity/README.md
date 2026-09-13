# Tutorial 26: Set Compose host identity

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`api` is a normal BusyBox HTTP image. `compose.yaml` gives it the hostname
`api.local` and a fixed `cache.local` entry. Theseus writes both into the
locked image runtime, checks them inside the guest, then replays the campaign.

Use literal DNS names and IP addresses. Theseus never asks a host resolver for
an `extra_hosts` address.
