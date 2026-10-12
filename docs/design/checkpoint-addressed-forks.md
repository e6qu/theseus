# Design: checkpoint-addressed forks

The counterfactual fork's remaining sliver. Today a fork re-executes the
recorded run's shared prefix deterministically instead of restoring the
original run's captured checkpoints. This document records why, what a
restore-based fork would require, and the tradeoffs, so the decision is a
review rather than a re-derivation.

## How a fork executes today

`--fork-run N` stages a replay plan whose campaign carries the
counterfactual override, plus the recorded campaign result. The runner
slices the recorded corpus to run N's schedule, applies the substitution,
and executes that one timeline through the ordinary campaign loop with the
`CampaignCheckpointTree` prefix machinery: the root boots from the
verified ready checkpoint, each operation boundary captures a prefix node,
and shared prefixes restore captured states.

The prefix tree is process-local by design - the captured memfds live and
die with the executing campaign. The forked process boots its own root and
**re-executes** the shared prefix, operation by operation. That is
deterministic: identical inputs, inputs order, and virtual time produce
the identical state the original run had at the same boundary. The
substituted decision then diverges the suffix.

## Why re-execution is the right default

- The recorded bundle carries the campaign result and plan, not the
  original run's memfds. Shipping them would grow bundles by the retained
  prefix memory (hundreds of MiB per branch) for every retained run.
- Retained RAM may contain secrets; a bundle that carries live guest
  memory for every run widens that surface from one exported root to
  every investigated run.
- Re-execution is cheap for real campaigns: the prefix is the
  deterministic part, and checkpoint-prefix reuse means the fork pays
  each prefix operation once (boots, UART writes, waits), not the guest's
  whole boot.

## When restore-based forks would win

- Long expensive prefixes: a setup that runs minutes of guest work before
  the interesting decision re-executes that setup on every fork question.
  Restore would make each follow-up question start at the decision.
- Interactive investigation: an investigator iterating substitutions at
  one boundary wants answer latency proportional to the suffix, not the
  prefix.

Both are real, and both arrive with the hosted/multi-question workflow
(Priority 6), where one campaign's evidence serves many fork questions.

## What a restore-based fork requires

1. **Retained prefix captures in the bundle.** The recorded run exports
   its prefix checkpoints alongside the result - the same ustar/memfd
   contract the ready-checkpoint root uses, keyed by boundary index, with
   the RAM/image identity digests the replay admission already checks.
2. **A retention decision per bundle.** Captures are opt-in
   (`--retain-prefixes` on the recording explore, or a compose flag), not
   a default: the secret-retention surface must stay a choice.
3. **Fork-side restore.** The staged fork plan names the boundary's
   capture; the runner boots from that capture instead of the root,
   splices the machine-stream prefix (the recorded stream up to the
   boundary, admitted as evidence), applies the substitution to the
   remaining schedule, and executes the suffix under the existing
   counterfactual contract.
4. **Provenance.** The forked result records
   `restored_from: {boundary, capture_sha256}` beside today's
   counterfactual provenance, and `compare --forked` reports which base
   the fork resumed from.

The replay-admission semantics do not change: the forked future already
does not inherit the recorded machine-stream gating; restoring only
changes where the deterministic execution begins.

## Recommendation

Keep re-execution as the default and the only implemented path today.
Add the retention option with the hosted workflow, when fork-question
latency becomes the measured pain - the design above is the
implementation contract. Nothing in the current fork, comparison, or
promotion surfaces changes.
