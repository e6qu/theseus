# Tutorial 10: Find a bad topology timeline

Run every command from this directory. This tutorial creates three tiny Linux
services. `api` accepts workload input. Theseus sends it named UART operations,
changes the simulated network or disk after selected operations, and checks a
runtime property from serial output.

```sh
export THESEUS_TAG=<12-character-sha>
export THESEUS_IMAGE=ghcr.io/e6qu/theseus:$THESEUS_TAG
docker run --rm --privileged -v "$PWD":/tutorial -w /tutorial \
  "$THESEUS_IMAGE" sh ./run.sh
```

Start with `compose.yaml`.

1. Set `driver` to the service that accepts workload input on `/dev/ttyS0`.
2. List operations as ordinary text. Theseus injects them into the driver UART;
   after each input it waits for `THES:CHECKPOINT:<operation-name>` before it
   injects the next one. No SDK or host-side wrapper is required.
3. List fault candidates. `partition` and `heal` change every simulated NIC on
   a named network. `storage_fault` changes one named simulated drive. Give
   these actions `after: <operation>`; Theseus applies them immediately after
   that operation reports its checkpoint.
4. Add properties. `always` needs every timeline to contain the assertion.
   `sometimes` and `reachable` need one witness. `unreachable` needs none.
5. Run `theseus compose explore`.

The API deliberately reports a stale read as
`THES:ASSERT:consistent_read:fail`. Exploration exits non-zero after writing a
locked campaign bundle. `run.sh` treats that failure as expected, renders a
report, and prints the failing property. The campaign result also records each
applied topology action. Replay checks that action sequence as well as the
normal serial, network, and storage evidence.

Then minimize it:

```sh
theseus compose explore --minimize theseus-compose-campaign \
  --output stale-read-replay
theseus compose replay stale-read-replay --output stale-read-rerun
```

Minimization re-runs complete locked topologies while removing operations. The
result is one ordinary Compose replay bundle with a check that proves the
counterexample still occurs.

Emit the serial protocol with plain shell:

```sh
printf '%s\n' 'THES:ASSERT:consistent_read:pass'
printf '%s\n' 'THES:CHECKPOINT:write'
```

The optional SDK provides the same lines through `TtyChannel::assertion` and
`TtyChannel::checkpoint`.
