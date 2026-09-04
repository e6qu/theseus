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
   a named network. `network_fault` changes selected packet conditions on every
   NIC on that network; set any of `drop_ppm`, `duplicate_ppm`, `corrupt_ppm`,
   `latency_rounds`, `jitter_rounds`, `tx_bytes_per_round`, `mtu_bytes`,
   `tx_queue_frames`, or `rx_queue_frames`. `network_recover` restores each
   service's declared network conditions. `storage_fault` changes one named
   simulated drive; `storage_recover` restores that drive's declared settings
   without discarding guest-written bytes. `packet_fault` drops only Ethernet
   frames with one `ethertype` (for example `0x0800` for IPv4); give it
   `drop_ppm`. Add `ip_protocol` and optional `source_port`/`destination_port`
   for IPv4 TCP or UDP headers. Add `from` and `to` to limit the rule to one directed service
   path. `packet_recover` removes that one matching rule. Give these actions
   `after: <operation>`; Theseus applies them immediately after that operation
   reports its checkpoint.
   `link_partition` and `link_heal` are narrower: give them `network`, `from`,
   and `to` to block or restore only that directed service-to-service path.
4. Add properties. `always` needs every timeline to contain the assertion.
   `sometimes` and `reachable` need one witness. `unreachable` needs none.
5. Set `max_faults_per_run` to explore candidate sequences. It defaults to 2
   and is capped at 4; `max_runs` remains the final bound on work.
6. Run `theseus compose explore`.

The API deliberately reports a stale read as
`THES:ASSERT:consistent_read:fail`. Exploration exits non-zero after writing a
locked campaign bundle. `run.sh` treats that failure as expected, renders a
report, and prints the failing property. The campaign result also records each
applied topology action. Replay checks that action sequence as well as the
normal serial, network, and storage evidence.

A candidate pair is one timeline. For example, a `network_fault` after `write`
and a `network_recover` after `retry` run together, in that operation order.
The recovery restores only packet conditions: a simultaneous partition or
directed-link action remains in force. This lets you test recovery without a
host-side test script.

`storage_fault` and `storage_recover` form the same pair for a drive. Recovery
does not roll back writes or reseed the I/O stream; it only restores the
conditions declared in that service's manifest.

`packet_fault` and `packet_recover` form a narrow network pair. They match the
Ethernet header only; they are not an IP, TCP, or payload filter. With `from`
and `to`, the switch applies the rule only to that directed path. Recovery
removes the selected EtherType rule while leaving partitions, directed links,
and ordinary packet conditions unchanged.

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
