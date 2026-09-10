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
2. List operations as ordinary text. Theseus injects them into the driver UART
   by default; set an operation's `service` to send it to another service's
   `/dev/ttyS0`. `replica_probe` is the example: it sends `probe` to `replica`,
   while `write`, `read_stale`, and `retry` go to `api`. Each target announces
   `THES:M:42`, then prints `THES:CHECKPOINT:<operation-name>` after its input.
   Theseus waits for that checkpoint before injecting the next operation. No SDK
   or host-side wrapper is required.
   Use `inputs` when one logical operation has several explicit payload cases.
   Use `input_grammar` when cases are a finite product. Set `template` with
   `{variable}` placeholders and map each variable to named `choices`.
   Theseus expands the product before boot, locks the concrete UART bytes into
   the replay plan, and reports each leaf as `operation[case]`. Set
   `name_template` to make stable case names from choice names. This tutorial
   expands `write {value} {durability}` into four leaves, including
   `write[beta-async]`, without duplicating its operation guards or faults.
   Add a `cases` entry to attach `requires`, `excludes`, `max_uses`, or state
   rules to one generated leaf. The grammar is capped at 64 leaves.
   Reference any logical operation as `write`, or one exact payload case as
   `write[beta-async]`.
   Quote an exact reference in a YAML flow list:
   `requires: ["write[beta-async]"]`.
   Use `input_template` when the next UART command needs a value the guest
   already emitted. Put each `{placeholder}` under `input_captures` with a
   JSON event predicate and pointer; set `service` to read another service,
   or omit it for the operation target. Set `select` to `first` or `latest` (the default)
   to choose a matching event from the restored parent transcript. The template
   placeholders and capture names must match exactly. This tutorial turns the
   write request ID into
   `retry transaction-42`. Theseus stores the rendered bytes in each run's
   replay plan, so that plan replays without re-reading a source Compose file.
   An `input_grammar` can use captures too: its choices still make stable case
   names while captures fill the shared fields at the restored checkpoint.
   Here the retry grammar explores `normal` and `force` modes with the same
   captured request ID.
   Use `workflow` when that transaction crosses services. Set shared
   `pointers`, then list ordered steps for each named service. A stage can
   override `pointers` when its JSON uses different field names. Theseus reads
   the capture pointer from the final stage only after one key completes every
   stage. This tutorial retries only a write that the auditor observed.
   Use `sequence` for the same ordered proof within one service: each item is
   a normal serial predicate, and Theseus reads the capture pointer from the
   final JSON item.
   Set a capture's `encoding` to `text` (the default), `json`, or `hex`
   when the guest protocol needs an unquoted scalar, a JSON literal, or
   hexadecimal text. JSON encoding also preserves a complete captured object
   or array; text and hex deliberately accept scalars only.
   Compose a capture's JSON event predicate with `all`, `any`, and `none` when
   it needs several alternative or forbidden conditions. Every branch tests the
   same JSON event; use `sequence` when the evidence spans several events.
   Use `query` for an RFC 9535 JSONPath expression when nested arrays or
   filtered descendants are clearer than several pointer predicates. A query
   matches when it selects at least one node from the same JSON event. This
   tutorial uses it to find the passing `serial` auditor check.
   These rules constrain the selected cases only; they compose with the
   operation-level state rules. Here `read_stale[after_beta]` can follow the
   `write[beta-async]` payload but not another write leaf.
   Use `state` to declare the initial finite-state model, for example
   `state: {phase: fresh}`. An operation or input case can require an exact
   value with `requires_state` and update it with `sets_state`. Theseus applies
   the logical operation update first, then the case update, so a case can
   specialize a shared transition. State guards prune impossible histories
   before a VM runs; they do not claim that the guest emitted that state. This
   tutorial moves `fresh → prepared → written → stale → recovered`.
   Add `requires: [operation-name]` when an operation needs one or more earlier
   operations. Theseus generates only histories where every requirement has
   already occurred; unknown, duplicate, and cyclic requirements are rejected.
   Add `excludes: [operation-name]` to block an operation after a named earlier
   operation, and `max_uses` (1 through 4) to cap its repetitions. These rules
   describe the valid workload state machine without a host-side driver.
   Add `requires_markers: [marker]` or `excludes_markers: [marker]` when the
   decision depends on what the guest actually reported. Theseus reads
   `THES:M:marker` lines from the restored parent checkpoint before extending
   it, so a blocked operation never consumes a campaign run.
   Use `requires_serial` or `excludes_serial` when the decision needs the
   full nested serial predicate. Theseus evaluates it against the driver's
   restored transcript, including one-line JSON event predicates.
   Set `service: <name>` inside that guard to read another service’s restored
   transcript instead; this joins deterministic evidence without a host-side
   coordinator.
   Use `requires_serial_all` to require several guards at once, potentially
   from different services. Use `excludes_serial_any` to block an operation
   when any listed guard matches.
   Use `stages: [setup, workload, recovery]` and give every operation a
   `stage`. Histories may stay in a stage or move forward, never back. This is
   the compact way to express a workflow without pairwise exclusions.
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
   for IPv4 or IPv6 TCP or UDP headers. Add `from` and `to` to limit the rule to one directed service
   path. `packet_recover` removes that one matching rule. Give these actions
   `after: <operation>`; Theseus applies them immediately after that operation
   reports its checkpoint. To fault only one payload variant, quote its case
   reference instead: `after: "write[beta-async]"`. The tutorial partitions
   the network after that generated beta write, while the logical `write` remains the
   checkpoint name.
   `link_partition` and `link_heal` are narrower: give them `network`, `from`,
   and `to` to block or restore only that directed service-to-service path.
4. Add properties. `always` needs every timeline to contain the assertion.
   `sometimes` and `reachable` need one witness. `unreachable` needs none.
   Use `contains_all`, `contains_any`, and `contains_none` for a flat
   predicate. Use `predicate` when you need nested `all`, `any`, or `none`
   groups. A leaf is `contains: <serial text>`, `matches: <Rust regex>`, or
   `json.fields`, a map of JSON Pointers to exact values. `json.fields` matches
   one complete JSON line, so its fields cannot come from separate events.
   Use `sequence` to require text, regex, or JSON leaves in transcript order.
   Each sequence item has exactly one of `contains`, `matches`, or `json`.
   In a JSON sequence item, use `capture: {name: /pointer}` to bind a value.
   A later JSON item can require that value with
   `equals_capture: {/pointer: name}`. The tutorial uses this to prove that
   the stale assertion belongs to the earlier write transaction.
   Use `occurs` to require one leaf exactly, at least, or at most a number of
   times. Put that leaf under `occurs.predicate`.
   Use `requires_serial_all` to join property evidence from named services.
   `requires_serial_any` needs one listed guard; `excludes_serial_any` rejects
   a property when one matches. Each guard uses `service` and the same serial
   predicate syntax as an operation guard.
   Use `requires_serial_correlations` when two services must report the same
   JSON value. Each entry has `capture` and `equals` endpoints. Give each a
   `json` event predicate and `pointer`; set `service` when it differs from
   the property service. The tutorial matches the API write transaction to the
   auditor's observation.
   Use `requires_serial_joins` when three or more endpoints must share one
   value. Put at least two endpoints under `endpoints`; Theseus looks for one
   value common to every endpoint, not independent pairwise matches. Use
   `pointers: [/request_id, /attempt]` for a composite key. The tutorial joins
   the API, replica, and auditor transaction events on both fields.
   A join defaults to `quantifier: any`: one shared key is enough. Set
   `quantifier: every` to require every key selected by the first endpoint to
   appear at every later endpoint. Use this to assert complete replication,
   not just one successful request.
   Add `occurs: {exactly: N}`, `at_least`, or `at_most` to bound matching
   distinct first-endpoint keys. Repeated JSON lines do not raise this count.
   Operations accept `requires_serial_joins` too. Theseus evaluates them at
   the restored parent checkpoint and skips an operation before it consumes a
   campaign run when the shared value is absent. Use `excludes_serial_joins`
   to skip it when a completed or forbidden transaction is already present.
   Use `requires_serial_evidence` when a rule combines these evidence types.
   Each node has exactly one of `all`, `any`, `none`, `guard`, `correlation`,
   `join`, `relation`, `path`, or `workflow`. A `guard` has `service` and the normal serial
   predicate; `correlation` and `join` use the endpoint forms above. A
   `relation` has `left`, `right`, and an `operator`: `equals`, `not_equals`,
   `greater_than`, `greater_than_or_equal`, `less_than`, or
   `less_than_or_equal`. Numeric operators compare one pointer on each side;
   equality operators can compare same-sized composite keys. Theseus evaluates
   the whole tree against the restored checkpoint before starting an operation.
   Relations also accept `quantifier: every` and `occurs` with the same
   distinct-left-value counting rule as joins.
   Add `order: before` or `order: after` when both endpoints select the same
   service transcript. This requires strict event order as well as the value
   relation; the stale assertion follows the write in this tutorial.
   A `path` follows one key through two or more JSON events in one service
   transcript. Give it `pointers`, ordered `steps`, and optional `service`,
   `quantifier`, and `occurs`. Each later step must carry the first step's key;
   the tutorial follows its request ID from write to stale-read assertion.
   A `workflow` applies keyed paths to two or more explicitly named services.
   Its first stage supplies the keys; every later stage must complete its own
   ordered steps for the same key. The replica and auditor streams have no
   shared clock, so the tutorial uses a workflow for their service-local stages.
   A stage can override `pointers` when its service stores the same key under
   different JSON field names; every stage must still provide the same tuple
   width. The replica and auditor demonstrate that mapping here.
   Properties also accept `requires_serial_evidence` and
   `excludes_serial_evidence`; the latter rejects a property when its tree
   matches. The retry rule combines an API assertion, auditor readiness, and
   the three-service composite join in one `all` expression. The stale-read
   property also requires its assertion attempt to be greater than the write
   attempt.
   Put reusable expressions under `campaign.evidence`, then use one anywhere
   in an evidence tree with `use: name`. Definitions can use earlier or later
   definitions, including inside `all`, `any`, and `none`; Theseus rejects
   unknown names and cycles. The locked plan expands every use, so a replay
   stays self-contained even if the source Compose file changes later.
   Add `json.where` for one condition per pointer: `equals`, `matches`,
   `greater_than`, `greater_than_or_equal`, `less_than`, `less_than_or_equal`,
   or `exists`. These conditions also match one complete JSON line.
   Use `json.arrays` for records inside one JSON array. Give each entry a
   pointer and exactly one of `any`, `all`, or `none`, then put another JSON
   predicate under that selector.
5. Set `max_operations_per_run` to explore every ordered operation history,
   including repeated operations such as retries. It defaults to 3 and is
   capped at 4. Set `max_faults_per_run` to explore candidate fault sequences;
   it defaults to 2 and is capped at 4. `max_runs` remains the final bound on
   executed work.
6. Run `theseus compose explore`.

The API deliberately reports a stale read as
`THES:ASSERT:consistent_read:fail`. Exploration exits non-zero after writing a
locked campaign bundle. `run.sh` treats that failure as expected, renders a
report, and prints the failing property. The campaign result also records each
applied topology action. Replay checks that action sequence as well as the
normal serial, network, and storage evidence.

A candidate pair is one timeline. For example, a `network_fault` after `write`
and a `network_recover` after `retry` run together, in that operation order.
Theseus explores operation histories breadth-first. It first tests one-step
operations, then pairs such as `write → read_stale`. Here `read_stale` needs
the `written` marker and `retry` requires the stale structured assertion, so
root reads and retries before a stale read are skipped from their restored parent states. Repeated
operations are valid unless `max_uses` limits them.
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

Theseus boots the complete topology once, then restores every candidate and
every minimization attempt from that same checkpoint. Theseus also captures
each distinct operation prefix after its UART barrier in an in-memory branch.
Schedules that share a prefix restore that node instead of re-running its
earlier operations. Linux shares its clean guest-memory pages and copies only
pages a child writes. The
reducer removes large contiguous chunks first, then narrows to individual
operations and selected topology faults while the same property still fails.
`minimization.json` records both sequences and the replay attempts used for
each pass. `theseus report stale-read-replay` shows the same evidence. The
result is one ordinary Compose replay bundle. For a compound property, Theseus
re-evaluates the complete predicate against its locked serial transcript after
every reduction attempt and in the final bundle.

The campaign report shows the **checkpoint economics** for the entire search:
root and prefix captures, prefix reuse, logical topology restores, retained
immutable memory, logical COW-mapped bytes, dirty pages at capture barriers,
and zero snapshot-file bytes. `avoided_prefix_recomputations` is the work
skipped because schedules shared a captured history. These are deterministic
work counters, not host timings, so they replay on any supported machine.

Each generated timeline also carries a **guidance ledger**: the number and
SHA-256 of completed observations that the scheduler saw before selecting that
timeline. This makes the selection auditable without embedding every earlier
observation repeatedly. Replaying a new bundle verifies both each ledger and
the final search-wide economics. Each run's `replay-plan.json` still contains
the full operation history and replays independently.

The report shows the operation model, including earlier-operation rules and
observed-marker and serial guards. It also counts both kinds of guard leaf skipped before a
campaign run started.

Theseus first runs every one-operation history without faults. It then gives
priority to untried schedules that extend an operation prefix which produced a
new `THES:M:` marker or a failure. The report records that selection reason and
the full candidate count.

It also treats a changed simulated drive, network traffic/payload fingerprint,
virtual clock, or paused guest program counter as a new topology state. A new
per-service execution locations sampled at deterministic device-exit quanta
also guide later extensions. The final paused PC is retained as a diagnostic,
but it is not the main execution-coverage signal.
This reaches divergent outcomes even when the guest prints the same markers.
The report resolves each sample against that service's locked kernel ELF and
shows `address → function + offset · file:line` when symbols and DWARF debug
data are available. It removes absolute build paths from source hints. The raw
address remains the replay identity; stripped or unmatched kernels show the
address alone. These are checkpoint samples, not a full instruction trace.
Ordinary serial output and declared fault names are not state coverage, so they
cannot create artificial novelty.

Choose the primary coverage signal with `coverage:`. The default,
`execution_locations`, uses the accumulated exit samples. Use `markers` for a
marker-only baseline or `checkpoint_pcs` for the final paused-PC baseline.
All three retain topology-state and failure evidence, use the same candidate
corpus, and lock the chosen mode into the replay bundle. That makes a campaign
comparison a change of one explicit input, not a different test.

Set `guidance: adaptive` to also rank a candidate's final operation by the
selected coverage signal, topology-state, and failure yield from earlier
completed runs. Theseus adds a declining exploration bonus for operations with
less evidence. The policy is deterministic: its recorded selection reasons
are replay-checked; it never calls a remote model or makes random choices.

Set `guidance: posterior` when you want the same evidence to be interpreted as
a deterministic Beta posterior. Theseus prefers evidence from the exact earlier
operation history, falls back to the action's global history when needed, and
keeps an uncertainty bonus so a thinly observed action still gets a fair trial.
The report records the prior-adjusted mean, uncertainty, yields, and misses for
every selected action. This is deterministic scheduling, not a remote model or
random sampling.

Set `guidance: property` to target your declared properties directly. A
`reachable` or `sometimes` match is a witness. An `always` miss or
`unreachable` match is a counterexample. Theseus first seeds every ordinary
operation, then prefers actions whose earlier executions produced witnesses in
the same operation history, falling back to that action's global evidence. It
keeps an exploration bonus for unseen actions, records the chosen reason and
per-run property witnesses, and replay-checks both. This is deterministic
property-directed scheduling, not a remote model or random sampling.

Read the **Operation boundaries** report section to follow a selected timeline
step by step. Each row is the paused checkpoint after one operation. It shows
the UART target, operation-local topology actions, markers, paused-PC locations, and a hash
of each service's serial transcript. Its delta says which markers are new and
which services changed serial output or paused-PC state since the preceding
checkpoint. **UART input** shows the exact delivered bytes as an escaped,
bounded excerpt with their byte count and full hash. **UART delivery** proves
the emulator accepted those bytes, shows how many the guest read, records the
UART FIFO depth before and after, and names the marker barrier it awaited.
**UART barrier** proves that marker appeared after the operation input, with a
bounded response excerpt and full response hash through the matched marker.
Theseus advances the simulated topology in numbered rounds while it waits, so
a response that needs the network can reach its barrier without host-time
polling.
Set `max_rounds` in each service manifest to bound the complete topology run
the same way; it replaces host elapsed time once the guests are running.
For a scheduled restart, the **Applied faults** report row also records the
deterministic barrier rounds used to reach the replacement's ready marker and
to replay its initial UART events. Use that number to see how much simulated
topology progress the restart required.
The locked replay plan retains the complete input. The report also shows new
serial bytes as an escaped excerpt, plus their byte count and full-delta hash.
The excerpts are capped; the complete serial logs and VM snapshots stay in
that locked run directory. These boundary rows also show the simulated-network
TX, RX, drop, duplicate, and corruption
counters produced by that operation. The boundary state hash covers this
compact evidence, so compare the same row across replays without opening a VM
snapshot. These rows explain execution; accumulated device-exit location
samples participate in coverage novelty. The row's scheduler round shows deterministic
progress without relying on host wall-clock time.

When an operation changes a simulated drive, **Changed storage** names that
service and drive. **Virtual time delta** shows each paused vCPU's elapsed
virtual nanoseconds. These values are captured at the same barrier as serial,
network, and instruction evidence. The browser report shows the same boundary
columns as the Markdown and machine-JSON reports.

When you replay a campaign bundle, Theseus does not search again. It restores
and runs the recorded operation and fault corpus in the recorded order, then
checks the selection reasons, marker novelty, topology-state signatures, and
applied actions. The report shows whether that campaign replay verified.

Emit the serial protocol with plain shell:

```sh
printf '%s\n' 'THES:ASSERT:consistent_read:pass'
printf '%s\n' 'THES:M:written'
printf '%s\n' 'THES:CHECKPOINT:write'
printf '%s\n' '{"event":"operation","name":"write","value":"beta","request_id":"transaction-42","attempt":1}'
printf '%s\n' '{"event":"assertion","name":"consistent_read","passed":true,"attempt":2,"request_id":"transaction-42"}'
```

The optional SDK provides the same lines through `TtyChannel::assertion` and
`TtyChannel::checkpoint`.
