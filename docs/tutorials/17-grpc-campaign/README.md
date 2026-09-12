# Tutorial 17: Campaign a gRPC health service

Run every command from this directory. You need Linux, KVM, and Docker. Set a
published Theseus runtime image:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`main.go` is a normal service exposing the standard clear-text gRPC health
method. `compose.yaml` declares one `grpc_health` operation. Theseus waits for
the service, sends the health request, records its outcome and checkpoint, and
replays the locked campaign.

Replace the image with your own service. Keep its standard health endpoint,
then change `url`, `service`, and `expect_status`. The application does not
need an SDK, a serial protocol, reflection, TLS, or an arbitrary RPC adapter.
