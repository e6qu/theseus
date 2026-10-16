## Summary

- **Stage 3 of deterministic timer arming — the wake design, probed through CI with a full finding**: in hold mode the arming write is recorded, KVM's hrtimer is never armed, and a wake thread sleeps the remaining guest interval then requests the timer wake; the run loop advances the virtual clock to the deadline (recorded jump) and queues the LAPIC-timer delivery
- **CI finding**: the recording delivers the first tick and the guest completes the sensor workload — but then hangs before re-arming/rebooting (guest_exit/completion fail). The wake cycle's first delivery works; the guest's progress after it does not. The interaction is in the re-arm cycle or the halt re-entry — a race/interaction that needs host-session debugging with the guest's own log (ttyS1 retention is in place)
- the arm write-through is restored on this branch (stage-2 behavior); the wake machinery remains in the code behind the same hold+filter conditions for the host session to iterate with

## Tests

- CI's native-KVM hold step with the wake design: first delivery lands, the workload completes, the hang is after it — the retained bundle (hold-replay-debug artifact) has both serial logs and the trace for the host session
- the branch restores the passing write-through path; fmt clean
