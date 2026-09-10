# Tutorial 11: Certify a deterministic runtime

Run every command from this directory. This tutorial runs one fixed topology
twice on real KVM. The second run replays the first and must match its serial,
entropy, storage, network, virtual-clock, and lifecycle evidence exactly.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
docker run --rm --privileged -v "$PWD":/tutorial -w /tutorial \
  "$THESEUS_IMAGE" sh ./run.sh
```

Read `certificate/certificate.json`. It is the support profile and the
repeatability witness for this exact plan. Certification refuses a plan without
virtual time, a non-KVM host, or a configuration that exposes host-backed I/O.
It proves exact end-of-run fingerprints, not every clock read within one
exit-counted virtual-time quantum.
