# Tutorial 19: Connect container services by name

Run every command from this directory. You need Linux, KVM, and Docker.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

The two Dockerfiles are ordinary BusyBox HTTP services. `compose.yaml` puts
them on `backplane`. Theseus gives each image a deterministic IPv4 address and
writes Compose service names into `/etc/hosts` before starting its entrypoint.

The `read_worker` command runs inside `api` and requests
`http://worker:8080/identity`. Neither image configures a NIC, runs DHCP, or
contains Theseus code. The script explores the one operation, checks the
request, and replays the locked campaign.
