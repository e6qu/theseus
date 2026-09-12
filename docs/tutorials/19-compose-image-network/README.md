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
Only `api` declares a readiness check; `worker` demonstrates that plain image
services receive this network setup too.

`api` uses Compose `depends_on` to start only after `worker` reaches its
deterministic boot barrier. Theseus records this relationship in the locked
plan; no sleep loop or guest-side wait script is needed.

`worker` reads its ordinary `IDENTITY` environment variable in its normal
entrypoint. The literal value comes from `compose.yaml`, is locked into the
derived image contract, and is returned by `read_worker`; Theseus never reads
an environment value from the host.

The `read_worker` command runs inside `api` and requests
`http://worker:8080/identity`. Neither image configures a NIC, runs DHCP, or
contains Theseus code. The script explores the one operation, checks the
request, and replays the locked campaign.
