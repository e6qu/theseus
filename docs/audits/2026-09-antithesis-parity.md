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
  overrides, guidance-and-budget-labeled progress streams, and
  alternative-futures failure frequencies in reports.

## Capability matrix

| Capability | Antithesis | Theseus | Gap |
|---|---|---|---|
| Deterministic execution | Custom hypervisor; instruction-level determinism for any x86 binary | KVM + exit-counted virtual time; recorded machine stream gates exits, device effects, explicit inputs, held timer turns | Guest instruction ordering, counter reads, and unheld in-kernel timer arming remain uncontrolled |
| Workload packaging | Docker Compose and Kubernetes (single-node K3s, `kapp` readiness, air-gapped) | Compose subset over images and manifests | No Kubernetes input; Compose breadth keeps growing |
| Test interface | Test templates (7 command types) + SDK assertions | Same template layout and lifecycle vocabulary; explicit operations; serial/JSON properties | Mature scheduler adaptivity over long hosted runs |
| Assertions | `always`, `alwaysOrUnreachable`, `sometimes`, `reachable`, `unreachable`; cataloging; default crash/OOM properties | All five quantifiers over serial/JSON evidence | No default crash/OOM properties; no cross-run property catalog keyed by assertion identity |
| Coverage | C/C++, Go, Java, JavaScript, Rust, .NET; `dlopen` DSOs; symbol ingestion required for properties | GCC C blocks, Go main-module blocks, LLVM C/C++/Rust edges; locked catalogs and source joins | Java, JavaScript, .NET, Go external modules and CGO, Rust dynamic graphs |
| Faults | Network partition/slow/jam/clog/restore; node stop/kill/pause/throttle; CPU modulation; clock; custom user-defined faults | Service lifecycle; partitions; degradation; clogs; storage; packet selectors; clock jumps backward/forward; clock-rate windows; CPU throttling | Custom user-defined fault interface; clock-rate reduction below 1x |
| Concurrency | Deep thread/process scheduling | Bounded instrumented GCC C pthread path | General Linux thread/process scheduling |
| Structured randomness | SDKs for Go, Java, C, C++, JavaScript, Python, Rust, .NET + JSONL | Bounded shell choices; Rust helper; language-neutral protocol | SDK breadth; immediate-use guidance feedback from generated values |
| Debugging | Time travel, multiverse map, causality analysis, streaming reports | Reports, replay, minimization, checkpoint export, violation context, alternative-futures frequencies | Interactive time travel; counterfactual re-execution; moment-addressed logs |
| Event logs | JSONL with `(vtime, input_hash)` moments; temporal operators; event sets | Ordered decision traces; serial deltas; violation excerpts | `(vtime, input_hash)` moment addressing; preceded-by/followed-by queries |
| Delivery | Hosted SaaS; REST API v0/v1; webhooks; Snouty CLI; CI triggers; integrations | Self-hosted CLI; markdown/JSON/JUnit reports; structured progress streams | Campaign API; notifications; parallel workers; live log/coverage view |

## Priority gaps and the fix for each

1. **Moment-addressed event log** (`(vtime, input_hash)`). Campaign
   boundaries now carry the moment address `<vtime_ns>@<input_sha256>`,
   giving reports and queries one stable point per operation. The open
   work is retrieval on that address: cross-run diffing landed (`theseus
   compare` reports both sides' moments at the diverging boundary) and
   campaign reports now carry a moment log indexing every address to its
   bounded excerpt. The open work is the query operators themselves
   (`preceded by` / `followed by` over retained events).
2. **Temporal queries** (`preceded by` / `followed by` over retained
   events). The property layer already evaluates temporal relations inside
   runs; exposing them as queries over a retained bundle reuses that
   matcher.
3. **Counterfactual re-execution** — fork a retained checkpoint, change one
   recorded choice or fault, re-execute, and diff the futures. Checkpoints,
   prefix reuse, and host-input replay exist; the missing piece is the
   plan-level override-and-diff workflow on top.
4. **Default properties** — automatic crash and completion verdicts per
   run (`theseus:crash`, `theseus:completed`) are recorded beside declared
   properties without user declaration. The open work is OOM detection and
   cross-run property history keyed by assertion identity.
5. **Custom fault interface** — a user-declared fault action (command +
   condition + duration) inside the generated profile model.
6. **CI surface** — a versioned campaign API over the existing evidence
   formats, webhook-style completion notifications, and a GitHub Actions
   trigger recipe.
7. **Coverage breadth** — Java, JavaScript, .NET, Go external modules/CGO,
   Rust dynamic graphs, chosen by real workload demand.
8. **Kubernetes input and parallel workers** — after the execution-side
   priorities; packaging breadth does not compensate for a missing
   execution capability.

## Non-goals kept

Open-source operation, self-hosting, and arm64 remain delivery choices,
not substitutes: none of them closes a capability above. Arm64 native
certification is still pending its labelled self-hosted KVM runner; the
amd64 evidence set is the current proof standard.

## Review trigger

Re-run this analysis when any Priority 1–3 item lands, when Antithesis
ships a capability this file does not model, or at the next milestone —
whichever comes first.
