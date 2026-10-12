# Antithesis parity analysis — September 2026

Where Theseus stands against the product it is modeled on, what is still
missing, and what to build next. This is a working analysis, not a
benchmark: Theseus has no head-to-head evaluation with Antithesis. Facts
about Antithesis come from its public documentation, retrieved this month;
facts about Theseus come from the repository and its retained evidence.

## What changed since the last analysis

This month closed or narrowed several named gaps:

- **Released-product proof.** Every SHA release is certified on native
  amd64 KVM: fixed-plan replay, the container/coverage/schedule/pthread/
  ordered-replay portfolio, and the retained distributed counterexample,
  all offline-verifiable through `theseus evidence verify`.
- **In-kernel timers** moved from invisible to observed (per-exit
  observation evidence) to held and replay-gated on amd64 (`hold_kernel_
  timers`).
- **Fault model** gained CPU throttling, directed link clogs, backward
  clock jumps, and clock-rate windows — as explicit faults and as
  `standard`-profile candidates, which was the Antithesis-shaped goal.
- **Assertions** gained `always_or_unreachable`; failed properties now lead
  to their violating timeline and a bounded serial excerpt.
- **Search comparison tooling** exists: `--max-runs`/`--guidance`
  overrides, guidance-and-budget-labeled progress streams,
  alternative-futures failure frequencies in reports, and
  `theseus evaluate compare`, which emits the side-by-side guidance
  artifact from retained campaigns, retained on native KVM for the public
  lost-update workload in every CI run. Cross-mode campaigns land as
  committed evaluation artifacts with the next release evidence set.
- **Investigation and delivery surface** grew an end-to-end reading loop:
  a read-only HTTP serve surface (registry, per-bundle evidence routes,
  rendered reports, live journals, moment and event queries, comparison
  and weighting summaries, Range-based tailing) with a stable campaign
  API record, a CI gate workflow, and a CI smoke script locking every
  documented route.
- **Weighted guidance** became evidence-driven: the guest journals (live
  progress, run records, checkpoint economics), the exploration journal,
  the budget sweep, and the partial-coverage reordering recording
  isolated the novelty mechanism, and the scheduler now consumes
  `--seed-choices` at the first decision — with four recorded
  observations and the multi-workload harness as the standing
  experiment.
- **Workload breadth** gained manifest directories (rendered Helm charts
  as campaign inputs), multi-container pods (per-container services with
  per-container manifests), tmpfs sizing enforced by the guest init, and
  a second bounded-choice workload feeding the weighting comparison.
- **Value predicates across moments** closed Priority gap 1: `query
  --where` composes the property layer's RFC 6901 fields shape with the
  needle relations, and every query predicate accepts the property
  layer's full grammar - RFC 9535 JSONPath selection, `where` conditions
  (equals, regex, comparisons, exists), array quantifiers, and nested
  `all`/`any`/`none` - evaluated in the property layer's order, on the
  CLI and on the serve routes through the same functions, so one grammar
  answers everywhere. Exports promote an answer's predicate into a
  standing property declaration beside a digest manifest.
- **Temporal operators** closed Priority gap 2's ordering work: `--within`
  bounds every relation to a boundary window (1 = the immediately
  adjacent boundary), `--anchor N` addresses one specific occurrence,
  and `--surrounded-by` answers the both-sides relation - and the
  property guards gained `sequence_windows`, the same ordered,
  byte-bounded windows evaluated over complete transcripts. The moment
  space and the property layer share one temporal vocabulary.
- **Counterfactual forks** closed Priority gap 3's choice and checkpoint
  work: `--fork-run N --set-choice OPERATION=INPUT` re-runs one recorded
  operation under another declared input case, and `--at-moment MOMENT`
  slices the forked future to the named boundary; `compare --forked`
  carries the provenance and diffs the re-run future like every fork.
- **Assertion suite auditing** narrowed Priority gap 4: exports carry the
  promoted property declaration, and `history --assertions --diff NEW`
  reports the identities a new campaign set added, retired, and flipped
  from always-passing to sometimes-failing.
- **The weighting experiment** exposed and fixed a real bug -
  `--seed-choices` was silently dropped beside `--expect-counterexample`,
  so the harness's seeded arms had recorded unseeded-vs-unseeded runs -
  and with real seeds now applied, all three bounded-choice workloads
  (six, six, and eight combinations) reproduce the fresh-first
  reordering on the first seeded run. The cross-workload summary carries
  the asserted yield comparison (witnesses-per-run delta, first witness
  position) the seed-default decision reads; the decision stays open
  pending yield divergence.

## Capability matrix

| Capability | Antithesis | Theseus | Gap |
|---|---|---|---|
| Deterministic execution | Custom hypervisor; instruction-level determinism for any x86 binary | KVM + exit-counted virtual time; recorded machine stream gates exits, device effects, explicit inputs, held timer turns | Guest instruction ordering, counter reads, and unheld in-kernel timer arming remain uncontrolled; the arming design is staged in [deterministic-timer-arming.md](../design/deterministic-timer-arming.md) |
| Workload packaging | Docker Compose and Kubernetes (single-node K3s, `kapp` readiness, air-gapped) | Compose subset over images and manifests; documented Kubernetes subset (Pods, Deployments, StatefulSets, DaemonSets, ReplicaSets, and Jobs - multi-container pods translate into per-container services; ClusterIP Services; ConfigMap/Secret volumes) translating into the same locked plan, from single files or rendered-chart directories (`helm template --output-dir` layouts) | Compose breadth beyond the audited subset ([2026-10 compose breadth audit](2026-10-compose-breadth.md)) |
| Test interface | Test templates (7 command types) + SDK assertions | Same template layout and lifecycle vocabulary; explicit operations; serial/JSON properties | Mature scheduler adaptivity over long hosted runs |
| Assertions | `always`, `alwaysOrUnreachable`, `sometimes`, `reachable`, `unreachable`; cataloging; default crash/OOM properties | All five quantifiers over serial/JSON evidence; default `theseus:crash`/`theseus:oom`/`theseus:completed` properties evaluated per campaign; cross-run assertion and choice catalogs (`history --assertions`, `history --choices`) with the set diff (`--diff NEW`: added, retired, flipped identities) | Catalog management workflows (hosted-surface scope; the identity keying and diff landed) |

| Coverage | C/C++, Go, Java, JavaScript, Rust, .NET; `dlopen` DSOs; symbol ingestion required for properties | GCC C blocks, Go main-module blocks, LLVM C/C++/Rust edges, Java class-load coverage via a `-javaagent`; locked catalogs and source joins | Java method-level probes, JavaScript, .NET, Go external modules and CGO, Rust dynamic graphs |
| Faults | Network partition/slow/jam/clog/restore; node stop/kill/pause/throttle; CPU modulation; clock; custom user-defined faults | Service lifecycle; partitions; degradation; clogs; storage; packet selectors; clock jumps backward/forward; clock-rate windows; CPU throttling; custom user-declared commands in image-backed services, also generated by the standard profile from each service's own commands | Clock-rate reduction below 1x |
| Concurrency | Deep thread/process scheduling | Bounded instrumented GCC C pthread path | General Linux thread/process scheduling |
| Structured randomness | SDKs for Go, Java, C, C++, JavaScript, Python, Rust, .NET + JSONL | Bounded shell choices; Rust helper with event export; Go, Java, and C/C++ modules with the same vocabulary including ordered event export; context-weighted guidance feedback from consumed values (per value-and-schedule-context novelty, recorded beside the run's choice records); cross-campaign correlation surfaces (`theseus history --choices` per-identity failure shares, `evaluate compare` per-identity outcome columns); seeded novelty (`compose explore --seed-choices` treats a prior campaign's consumed identities as known); language-neutral protocol | Guidance weighting signals beyond first-seen novelty. Landed: first-decision seed consumption (ordering test pinned), the ranking harness ([compare_weighting_signals.sh](../../scripts/compare_weighting_signals.sh), run in CI's native-KVM job with the `weighting-experiment` artifact retained), five recorded observations (saturation, prefix identity, mechanism isolation, partial-coverage reordering, and the three-workload fresh-first reordering with real seeds after the silent-drop fix), and the asserted yield table. Remaining: yield divergence - the seed reorders, and the decision reads the witnesses-per-run delta every CI run tabulates. The seeded ordering is operational end to end — documented in the structured-choices guide, exercised by tutorial 36's continuation step and ci-gating's gate workflow ([ci-gating.md](../ci-gating.md)) |
| Debugging | Time travel, multiverse map, causality analysis, streaming reports | Reports, replay, minimization, checkpoint export, violation context, alternative-futures frequencies, counterfactual forks - fault substitutions, choice substitutions, and moment-addressed slices - with moment-addressed diffs, digest-verifiable answer exports that promote into property declarations, moment-scoped artifact bundles | Interactive time travel; forked execution that starts at the checkpoint rather than re-running from the shared prefix |
| Event logs | JSONL with `(vtime, input_hash)` moments; temporal operators; event sets | Ordered decision traces; serial deltas; violation excerpts; moment addressing with needle relations, `--events` listing, guest-event moment-log rendering, event-field temporal relations, and the shared full predicate grammar (JSONPath, where conditions, array quantifiers, nested groups) with windowed, occurrence-addressed, and bracketing relations on the CLI and serve routes; sequence windows over complete transcripts in the property guards; Go/Java SDKs emitting guest events onto the same timeline | Event sets; transitive between-moments relations (undemanded) |
| Delivery | Hosted SaaS; REST API v0/v1; webhooks; Snouty CLI; CI triggers; integrations | Self-hosted CLI; markdown/JSON/JUnit reports; structured progress streams with a live journal (serve route, Range polls, `query --collect` prefix, status summary) | Campaign API; live log/coverage view while running |

## Priority gaps and the fix for each

1. **Moment-addressed event log** (`(vtime, input_hash)`). Campaign
   boundaries carry the moment address `<vtime_ns>@<input_sha256>`;
   retrieval is complete — cross-run diffing (`compare --at-moment`),
   moment-log rendering with guest events, previous/next navigation,
   full enumeration, and collect-able point bundles — and the reading
   paths are documented across CLI, HTTP, and offline artifacts. The
   value-predicate work is closed: `query --where` composes the
   property layer's fields shape with the relations, every query
   predicate accepts the property layer's full grammar through the same
   evaluation order on both surfaces, and exports promote an answer's
   predicate into a standing property declaration.
2. **Temporal queries** (`preceded by` / `followed by` over retained
   events). Landed at the moment-address tier, including event-field
   relations (`--preceded-by-event`/`--followed-by-event` over RFC 9535
   pointers with `where` comparators and nested groups), the shared full
   grammar, and the richer operators: `--within` windows, `--anchor`
   occurrence addressing, `--surrounded-by` brackets - and
   `sequence_windows` evaluate the same ordered, bounded steps over
   complete transcripts in the property guards. The undemanded residue
   is transitive between-moments relations and event sets.
3. **Counterfactual re-execution** — fork a retained checkpoint, change one
   recorded choice or fault, re-execute, and diff the futures. Landed for
   fault decisions (`--fork-run N --replace-fault OLD=NEW`), for choices
   (`--set-choice OPERATION=INPUT` re-runs one operation under another
   declared input case), and for checkpoints (`--at-moment MOMENT` slices
   the forked future to the named boundary); `compare --forked` reports
   the first diverging boundary with both sides' moments and carries the
   provenance for all three forms. The remaining sliver - execution
   starting at the nearest checkpoint rather than re-running the shared
   deterministic prefix - is analyzed with its tradeoffs in
   [checkpoint-addressed-forks.md](../design/checkpoint-addressed-forks.md);
   re-execution stays the default until fork-question latency is the
   measured pain.
4. **Default properties** — automatic crash, completion, and OOM verdicts
   per run (`theseus:crash`, `theseus:completed`, `theseus:oom`) are
   recorded beside declared properties; the OOM verdict classifies the
   kernel's own out-of-memory serial evidence, and the campaign API
   record carries the run-level summary a gate reads. Cross-run property
   history and the assertion/choice catalogs landed (`theseus history`,
   `--assertions`, `--choices`), the suite diff landed (`--diff NEW`:
   added, retired, flipped), and exports promote findings into
   declarations. The remaining work is catalog management UX on a hosted
   surface - Product-operation scope.
5. **Custom fault interface** — landed: `kind: custom` campaign faults run
   a user argv inside an image-backed service at an operation barrier
   through the pivot shell protocol, and the standard profile proposes
   custom candidates re-running each image service's own commands at
   eligible barriers.
6. **CI surface** — landed beyond the CLI: a versioned campaign API record
   (`GET /api/campaign/<name>`, `theseus-campaign-api-v1`) over the
   evidence formats, a route manifest pinning every serve route, a CI
   smoke script locking them, completion notifications, and a documented
   gate workflow asserting the record in a plain runner. A hosted
   campaign service remains Product-operation scope.
7. **Coverage breadth** — Java, JavaScript, .NET, Go external modules/CGO,
   Rust dynamic graphs, chosen by real workload demand. The representative
   Java path landed: `theseus coverage java` builds a generic class-load
   agent for one selected JAR, locks a class-to-offset symbol map, and the
   runner validates and symbolizes it like every other catalog. Method-level
   Java probes remain open.
8. **Kubernetes input and parallel workers** — packaging breadth does not
   compensate for a missing execution capability. Parallel workers landed
   at the CLI level (`--shard INDEX/TOTAL`), Kubernetes input landed for
   single-container Pods through Jobs from files or rendered-chart
   directories, and a hosted campaign service remains open.

## Non-goals kept

Open-source operation, self-hosting, and arm64 remain delivery choices,
not substitutes: none of them closes a capability above. Arm64 native
certification is still pending its labelled self-hosted KVM runner; the
amd64 evidence set is the current proof standard.

## Review trigger

Re-run this analysis when any Priority 1–3 item lands, when Antithesis
ships a capability this file does not model, or at the next milestone —
whichever comes first.
