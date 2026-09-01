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
