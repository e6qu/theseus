# Advanced guide: Explore branch timelines

Branch only after you can reproduce one useful run. A branch point captures a
VM's state; child timelines resume from it with their own seed and fault
schedule.

Use the Linux+KVM container from tutorial 1, then run these checks from
`/theseus/orchestrator`.

## 1. Verify seed isolation

```sh
cargo test --lib spawn::tests::test_branch_children_diverge_only_by_seed
```

The children receive different fresh entropy streams. Neither receives the
parent's continuation.

## 2. Verify memory isolation

```sh
cargo test --lib spawn::tests::test_branch_children_memory_is_cow
```

A write in one child does not change its sibling or the branch point.

## 3. Verify fault isolation

```sh
cargo test --lib spawn::tests::test_spawn_child_with_fault_schedule
```

The children receive separate drop and partition schedules.

Use branching to explore several futures from one interesting checkpoint.
See [exploration](../../exploration.md) for the data model.

## 4. Fork one recorded decision

The same idea works on retained campaign runs without touching engine
internals: `theseus compose explore --fork-run` re-executes one recorded run
from its shared deterministic prefix with exactly one decision changed, and
`theseus compare --forked` diffs the two futures at the first diverging
boundary. Three substitutions exist:

```sh
# Replace one recorded fault decision with another declared fault.
theseus compose explore --fork-run 4 \
  --replace-fault 'backplane:partition@verify=backplane:heal@verify' \
  campaign --output campaign-forked

# Re-run one operation under another declared input case.
theseus compose explore --fork-run 4 \
  --set-choice 'calculate=mode-0' campaign --output campaign-forked

# Slice the forked future at a named moment (with --set-choice).
theseus compose explore --fork-run 4 \
  --set-choice 'calculate=mode-0' --at-moment '9000@input-hash' \
  campaign --output campaign-forked

theseus compare --forked campaign campaign-forked
```

The fork locks its substitution into the forked replay plan and records
the provenance, so the comparison locates the base run without positional
guessing and reports both sides' moment addresses. A divergence supports -
but does not by itself prove - a causal claim. The fork forms are
documented in full in [exploration](../../exploration.md#fork-one-decision-
and-diff-the-futures) and the [CLI reference](../../../cli/README.md).
