# Tutorial 15: Campaign an unmodified container service

Run every command from this directory. You need Linux, KVM, and Docker. Set a
published Theseus runtime image:

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
sh ./run.sh
```

`Dockerfile` is an ordinary BusyBox HTTP service. `compose.yaml` declares a
GET request under `campaign.operations[].http`. Theseus injects that request
through its image pivot after readiness, records its result and checkpoint,
then replays the locked campaign. The image does not read a serial protocol or
link an SDK.

Use `service` to direct a request to another image-backed service. The URL is
the service's ordinary in-guest HTTP endpoint. Keep campaign properties about
the response evidence Theseus records on serial output.
