# Design: deterministic in-kernel timer arming

The last uncontrolled input on the held-timer path. This document records
the mechanism analysis, the options with their tradeoffs, the recommended
staged plan, and the acceptance contract, so the next KVM session
implements directly instead of re-deriving.

## Current mechanism

KVM's in-kernel LAPIC arms a host hrtimer when the guest writes the timer
initial-count register: xAPIC MMIO at `0xFEE00380` (TMICT) or x2APic MSR
`0x838`. On host-timer expiry, KVM asserts the LVT timer vector in the IRR
inside the irqchip - no KVM exit, invisible to the machine stream.

Theseus closes most of that gap today (`hold_kernel_timers`, amd64):

- `Vcpu::observe_kernel_timer` reads the LAPIC state at every handled exit
  and records each asserted episode as observation evidence
  (`vcpu:N:timer:lapic-timer`).
- In hold mode, `hold_lapic_timer` clears the vector's IRR bit so KVM does
  not deliver asynchronously, queues the delivery as a recorded vCPU turn,
  and replay admits it like every other stream effect
  (`inject_held_kernel_timer`).

## The residual nondeterminism

Arming. The guest computes the initial count from its own counter reads -
the TSC, which Theseus already virtualizes - but KVM turns that count into
a **host-time** hrtimer deadline at the write instant. Whether the delivery
is asserted at a given handled-exit boundary therefore depends on where
host wall-clock sat between the arming write and that boundary. Two replays
of one recorded execution whose exit boundaries drift relative to host
time observe different asserts and diverge. By design this divergence is
loud: held mode never passes silently on moved episodes.

## What deterministic arming means

The deadline must be expressed in the guest's own time base - the exit-
counted virtual clock - not host time. Concretely: intercept the arming
write, record it as a machine-stream decision
(`vcpu:N:timer:arm:<count>`), compute the period the guest asked for in
virtual nanoseconds, and assert the delivery at the first handled-exit
boundary at or after `arm_vtime + period`. The hold-and-inject path stays
exactly as today; only the *scheduling* of the assert changes. Host time
leaves the equation entirely, and the episode-movement replay flips from
expected-divergence to expected-match.

## Options

### A. Force x2APIC and filter the timer MSRs (minimal interception)

`KVM_CAP_X86_USER_SPACE_MSR` plus `KVM_X86_SET_MSR_FILTER` routes the
x2APIC timer MSRs - `0x838` TMICT, `0x839` TMCCT, `0x83E` TDCR - to
userspace instead of the in-kernel APIC.

- Force the guest into x2APIC mode by intercepting `IA32_APIC_BASE`
  (present x2APIC in CPUID is not enough; the OS decides by MSR write).
  Linux switches when it sees the CPUID bit and the base MSR bit, so both
  need steering.
- Theseus then owns three registers: the arming write (record + schedule
  the virtual deadline), the current-count read (answer from the virtual
  countdown - the guest polls TMCCT in some drivers), and the divide
  configuration.
- One-shot and periodic LVT timer modes both reduce to re-arming at each
  virtual deadline; periodic just reloads `count` from the last arm.

Tradeoffs: small KVM-surface change (a filter setup + three MSR handlers),
no new APIC emulation, but it only works for guests that accept x2APIC.
Bare-metal-style guests already programmed against xAPIC MMIO need the
CPUID/base steering verified per kernel; that verification is KVM-session
work.

### B. Userspace APIC (the end state)

Drop the in-kernel APIC and emulate the local APIC in Theseus: EOI, ICR
broadcasts, LVT lines, the timer, TMCCT - everything - against the virtual
clock, injecting through the existing `KVM_INTERRUPT` path.

Tradeoffs: removes the whole class (the timer stops being special), but it
is a large, subtle surface (ICR deliveries across vCPUs, EOI semantics,
level-triggered lines) and it changes injection ordering interactions with
the recorded interrupt turns. Not a first slice.

### C. Re-arm at boundaries without interception (rejected)

At each handled exit, read TMICT/TDCR from the LAPIC page, compute the
virtual countdown, and clear the in-kernel timer so its hrtimer never
fires - injecting virtually instead. Rejected: the guest's own TMCCT reads
still come from KVM's hrtimer state (unintercepted), so the guest observes
a countdown inconsistent with the deliveries it receives. Worse than
today's honest divergence.

## Recommended plan

1. **Stage 1 - interception spike (landed).** The MSR filter is on for
   the UART/reset KVM step (tutorial 4's `hold.toml`): the guest is
   steered into x2APIC, and every timer-MSR access exits, logs with its
   virtual timestamp, and is answered write-through. **Finding:** Linux
   steers around TMICT entirely - with the TSC-deadline CPUID bit set it
   arms through MSR `0x6E0` (MSR_IA32_TSC_DEADLINE), so the filter covers
   that MSR too and the observed arming writes land there. The guest
   boots, the UART workload passes, and the arming writes appear in the
   Firecracker log with their virtual timestamps. Read the logged
   pattern from the retained `firecracker.log` in the CI artifacts.
2. **Stage 2 - the arm decision (landed).** In hold mode the
   intercepted arming write is recorded in the machine stream as
   `vcpu:N:timer:arm:<msr>:<value>` (TSC-deadline or TMICT), admitted by
   replay through the shared override application. CI's hold step now
   records ~11 arms per boot and the held-delivery divergence tolerance
   accepts the arm as the named decision. **Observed:** the replay of a
   hold recording still fails (the guest exits nonzero) - the arm
   records are admitted but the *deadline values* are not yet
   deterministic, because the guest computes them from its TSC, whose
   reading between exits is not fully virtualized until stage 3. The
   tamper test (a forged timer turn) is still rejected.
3. **Stage 3 - the countdown and TMCCT (attempted twice; the halt
   frontier).** Two implementations, two blockers, one root cause:

   - **Virtual-deadline assert**: arm a virtual TSC deadline and assert at
     the first handled exit whose virtual TSC reaches it. Blocker: an
     idle guest arms the deadline and **halts** - exit-counted virtual
     time advances only on handled exits, so the virtual TSC freezes
     below the deadline, no boundary ever reaches it, and the guest hangs
     until the watchdog.
   - **Immediate delivery (halt-time policy: idle consumes no time)**:
     deliver the interrupt at the arm itself. Blocker: **interrupt
     storm** - the kernel's timer handler re-arms after each tick, and
     immediate delivery re-fires instantly, so the guest lives in an
     interrupt livelock and exits nonzero (soft-lockup territory).

   Root cause for both: with an in-kernel irqchip, KVM handles HLT
   in-kernel - the halted vCPU blocks inside KVM_RUN, produces **no
   handled exits**, and userspace never sees the halt boundary. The
   timer must fire *while the guest is halted*, and neither virtual-time
   assertion nor immediate delivery can cross that boundary. Stage 3
   therefore requires hypervisor-level halt interception (making HLT a
   handled exit in the Firecracker fork) so the halt position becomes a
   recorded, virtual-time-advancing event. That is the guest-side input
   consumption frontier the roadmap names, and it gates stages 3-4. The
   attempted implementations are reverted; the recording path stands at
   stage-2 behavior (host hrtimer arms; hold diverges on movement; the
   tamper contract is "the tampered bundle does not replay cleanly").
   The first implementation armed a virtual deadline and asserted at the
   first handled exit whose virtual TSC reached it. CI showed why that is
   not enough: an idle guest arms the deadline and **halts** - and
   exit-counted virtual time advances only on handled exits, so the
   virtual TSC freezes below the deadline, no boundary ever reaches it,
   and the guest hangs until the watchdog (the recording run failed its
   guest_exit/completion checks). The honest finding: the LAPIC timer
   must fire *while the guest is halted*, which exit-counted virtual
   time does not model. The stage-3 design is therefore the halt
   policy: intercept the halted state (KVM mp-state HALTED + a wake
   timerfd), and let a halt **consume virtual time up to the next armed
   deadline** - a deterministic policy, since the halt position is
   deterministic. That is the guest-side input consumption frontier the
   roadmap names, and it is the prerequisite for stages 3-4. The
   attempted implementation is reverted; the recording path is restored
   to stage-2 behavior (host hrtimer arms, hold diverges on movement).
4. **Stage 4 - flip the contract.** The episode-movement replay that
   currently expects divergence becomes an expected-match certification
   case; `hold_kernel_timers` stops being opt-in for new plans and the
   observation-only path retires to arm64 pending the same work there.

## Acceptance contract

- Same recorded execution + same arm decisions = same assert boundaries,
  on any host, at any host speed.
- The machine stream carries every arming write; replay rejects a
  changed count, divide configuration, or LVT mode.
- The guest's TMCCT reads agree with the deliveries it observes.
- No host-time reads on the timer path (grep-verifiable: the LAPIC timer
  code touches no `Instant`/`SystemTime`).
