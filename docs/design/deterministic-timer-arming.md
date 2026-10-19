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

### B. Userspace APIC (the end state, the implementation contract)

Drop the in-kernel irqchip entirely - no `KVM_CREATE_IRQCHIP` - and emulate
what the guest needs in Theseus, injecting through `KVM_INTERRUPT`. Without
an irqchip KVM still runs the vCPU: `KVM_INTERRUPT` injects any vector at
the guest's interrupt window (the original pre-irqchip semantics, exactly
the primitive the IRR probe proved the in-kernel irqchip withholds), and
`RFLAGS.IF` gating stays KVM's job.

The emulation surface for this workload (one vCPU, Linux, x2APIC):

- **Local APIC over x2APIC MSRs.** The existing steering already presents
  x2APIC; widen the MSR interception from the timer registers to the whole
  local-vector range (0x800-0x83F) plus the TSC-deadline MSR. State: the
  spurious vector + enable, TPR (0x808), EOI (0x80B, edge-timer no-op but
  recorded), ICR (0x830; self-IPI for one vCPU), SELF_IPI (0x83F), LVT
  timer (0x832), TMICT (0x838), TDCR (0x83E), TMCCT (0x839, read from the
  virtual clock), TSC-deadline (0x6E0). The timer is the design's core:
  the deadline lives in the virtual clock, asserted at the first handled
  exit at or after `arm_vtime + period` - no host hrtimer anywhere.
- **IOAPIC over its MMIO page.** GSI-routed devices (the UART on IRQ 4,
  the keyboard on IRQ 1) reach the guest through the IOAPIC the ACPI
  tables declare at 0xFEC00000: emulate the IORESEL/IOWIN register pair
  and 24 RTEs (vector, delivery mode, level/edge, mask). A device fire
  asserts its pin; an unmasked, edge- or level-asserted RTE delivers its
  vector through `KVM_INTERRUPT` at a recorded turn.
- **What KVM keeps:** interrupt-window gating, TSC/kvmclock virtualization,
  the vCPU run loop, HLT blocking (woken by a pending `KVM_INTERRUPT`).

Tradeoffs: removes the whole class (the timer stops being special; every
register access is a recorded stream turn), but it is a large, subtle
surface (ICR deliveries across vCPUs, EOI semantics, level-triggered
lines) and it changes injection ordering interactions with the recorded
interrupt turns. Not a first slice - it is the final one, staged behind a
flag, iterated with the kernel's own account on the COM2 console.

**PR sequence:** (1) the flag + no-irqchip VM creation + the widened MSR
interception with a state-carrying LAPIC + the IOAPIC with the serial
path, the guest booting as far as the COM2 log names; (2) the timer
delivery at virtual positions, replay-admitted; (3) the remaining
registers the kernel's account names, then the flag flip and the stage-4
contract (episode-movement replay expects a match).

**First boot against the userspace APIC (PR 1, landed):** the kernel
boots xAPIC (the x2APIC CPUID bit is actively cleared - KVM's supported
CPUID reports the host's bit otherwise), probes the emulated IOAPIC
(`IOAPIC[0]: version 17, GSI 0-23`), configures the local APIC entirely
through the emulated MMIO page (the register accesses that #GP'd as
x2APIC MSRs now serve cleanly), arms the TSC-deadline timer, and takes
ticks: `timer-due vector=236`, the kernel re-arms - the
arm -> virtual-deadline assert -> `KVM_INTERRUPT` -> re-arm loop runs
end to end. Two integration lessons are on record: the bus stores weak
handles, so the MMIO adapters need strong keepalive owners, and the
timer's deadlines must ride the virtual clock's nanoseconds (the guest's
TSC is periodically rewritten at quantum boundaries, so TSC-tick
absolutes break at every rewrite). **The current frontier:** the timer
   loop runs (arm -> virtual-deadline assert -> `KVM_INTERRUPT` ->
   re-arm), and the halt boundary is handled the design's way - the
   halt consumes virtual time up to the next deadline (a recorded
   `virtual_time_jump`) and the tick delivers at that position, so a
   halted guest cannot deadlock on a clock that only advances on
   handled exits. A later vmentry then fails with
   `KVM_EXIT_FAIL_ENTRY` (invalid guest state, 0x80000021), and the
   boot-time state dump names the shape: `ss=0` where the kernel's
   `__KERNEL_DS` (0x18) belongs, with interrupts disabled and a
   kernel-text RIP - the guest's stack segment is lost across the
   delivery + kick sequence on the no-irqchip vCPU. The next iteration
   dumps the segment attributes and the vCPU events beside the
   registers to decide between a KVM unrestricted-guest quirk and an
   injection-path state save. The MOV-SS refusal resolved as designed:
   the injected tick is withdrawn through the vCPU events and re-queued
   at the next boundary. The deeper finding followed immediately: the
   tick stream dies at ~10 ticks, every run, at the same kernel line -
   **the kernel's LAPIC-clock calibration measures the TSC delta across
   ticks, and the quantum-boundary TSC rewrites distort that measure**,
   so the calibrated period is wrong and the clockevents core stops
   trusting the device. The userspace APIC therefore needs its time
   base unified first: either the guest's TSC free-runs (a probe-mode
   cadence, host-paced) or the virtual-clock reads reach the guest's
   own timer arithmetic (the design's next slice) so calibration
   measures the virtual rate. That unification is the remaining work
   before the flag can flip; it ships off meanwhile, and the default
   in-kernel-irqchip paths are untouched and green.

   **The free-running-TSC probe (landed):** with the TSC free-running
   the kernel calibrates correctly - the ticks flow at their programmed
   10M-tick cadence (`timer-due ... tsc_now=923460297` ->
   `933518625` -> `943606500`, exact 4ms intervals), the halt boundary
   wakes by the arm-scheduled host wait, and both delivery primitives
   (KVM_INTERRUPT and the vCPU events) are exercised. The boot then
   stalls deterministically in the silent initcall stretch after
   `ip_rt_init` (the last visible line: `IP idents hash table
   entries`), with the tick stream alive and no lockup warnings -
   and every boot-time diagnostic is compiled out of the shipped CI
   kernel: `initcall_debug` prints through pr_debug (no
   CONFIG_DYNAMIC_DEBUG), and `hung_task_timeout_secs` is refused as an
   unknown parameter (no CONFIG_DETECT_HUNG_TASK). The blocking
   initcall therefore cannot be named from boot args; the probe needs
   the guest kernel rebuilt with dynamic debug or the hung-task
   detector (the config fragments live in
   `firecracker/resources/guest_configs/`; the vmlinux itself is built
   outside this repository and ships in the release). Two further
   probes sharpened it: the runtime pin in CI pointed at an older
   release whose kernel predated the config, and after advancing the
   pin the new kernel accepted `hung_task_timeout_secs` - and no
   warning fired, which clears the uninterruptible-wait hypothesis:
   **the blocked init is sleeping interruptibly, waiting for a wakeup
   that never comes** - plausibly an IRQ the userspace APIC's IOAPIC
   path owes it. The definitive probe is repo-side: enable
   CONFIG_DYNAMIC_DEBUG in the x86_64 6.1 config (which the release's
   Docker build already compiles), publish, advance the pin, and the
   `calling initcall` sequence names the blocking call. The flag ships
   off; the default in-kernel-irqchip paths are untouched and green.

   **The dynamic-debug probe (landed):** the pin advanced to the
   dynamic-debug kernel and `initcall_debug dyndbg="file init/main.c
   +p"` and the global `dyndbg=+p` both ran - and no `calling initcall`
   line printed either way, while the boot otherwise looked normal
   (no debug flood). The 6.1 initcall-tracing prints evidently do not
   reach the console through the dyndbg machinery in this build. The
   remaining repo-side move is a kernel patch: `rebuild.sh` already
   applies `firecracker/resources/patches/<version>/*.patch` before
   building, so a patch switching do_one_initcall's tracing prints
   from pr_debug to printk(KERN_INFO) makes `initcall_debug` work
   unconditionally on the next release. That patch is the next
   session's opener.

   **Solved without the patch (the loglevel off-by-one):** the 6.1
   initcall tracing prints `printk(KERN_DEBUG)` - level 7 - and
   `loglevel=7` suppresses exactly level 7 (the console shows levels
   strictly below it). `loglevel=8 initcall_debug` printed the full
   calling/returned trace on the unmodified kernel, and the trace
   named the stall: `calling inet_init+0x0/0x296`, never returning,
   while the periodic tick ran and the guest EOI-ed every tick (the
   machine stream shows the `lapic-timer`/EOI loop). **`highres=off`
   unblocked it**: the hrtimer high-resolution switch is what breaks
   under the userspace APIC - with it off the boot passes inet_init
   and the entire core-initcall sequence (~390 initcalls traced), and
   stalls next at the ACPI `ged_driver_init` (the Generic Event
   Device's GSI 6 path - the IOAPIC RTE for that pin is never
   programmed before the stall). The userspace APIC's remaining work
   is therefore the clock-event/hrtimer contract (the oneshot
   retrigger semantics the high-res switch relies on) and the Ged's
   interrupt path, both kernel-visible now. The flag ships off; the
   default in-kernel-irqchip paths are untouched and green.

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
   The path forward is admission-side (see the candidate resolution
   below): write-through arming stays the wake, the arm record stays
   the replay-admitted decision, and the delivery admits only against
   its own arm - which is landed.

   **The refined stage-3 contract** (the wake-timer design) — implemented
   behind the experimental `halt_timer_delivery` flag: the HLT exit is
   handled (dormant until HLT surfaces through KVM's in-kernel halt
   emulation); the arm write stays write-through so KVM's host hrtimer
   remains the *wake* source; a wake thread sleeps for the remaining
   virtual interval, requests a timer wake (kicking the halted vCPU out
   of KVM_RUN), and the run loop advances the virtual clock to the armed
   deadline — recorded as a `host:virtual_time_jump:+N` turn (the halt
   consumed that virtual time) — then queues the delivery turn. A
   deadline that passed during the wake gap fires immediately without a
   jump. The wake's host timing does not enter the stream: the jump lands
   at the deterministic deadline and the delivery turn follows it.

   **Open frontier (KVM-host iteration) — the precise statement:** the
   retained hold-replay trace shows **66 arm records and zero
   lapic-timer delivery records**: KVM's in-kernel irqchip injects the
   lapic-timer interrupt directly (waking the halted vCPU, advancing the
   guest's view of time) without a userspace-visible turn, so the hold
   path's clear-IRR-and-queue almost never catches one. The hold-mode
   determinism claim therefore reduces to the assert *timing* (a moved
   episode diverges at the arm records' admission), not the deliveries —
   and the reboot-path hang under the halt-time delivery is the same
   in-kernel delivery interacting with the reboot path's expectations.
   The real fix is the userspace APIC (option B above): with the LAPIC
   emulated in userspace, arms, asserts, and deliveries all become
   recorded stream turns. That is a large, deliberate slice for a
   KVM-host session; the arm records (stage 2) are its foundation and
   are shipped and green.

   The debug-console wiring (COM2 device, checkpoint-path serial config,
   validator admission) and the reboot-point marker are the standing
   diagnostics: every future probe retains both serial logs and pins the
   hang position on the ttyS0 timeline.

   **The wake design probed through CI (third attempt):** recording the
   arm, skipping write-through, and sleeping the remaining guest interval
   in a wake thread delivers the **first tick** — and the guest then
   hangs before re-arming or rebooting (guest_exit/completion fail). The
   first delivery works; the guest's progress after it does not. The
   write-through arming was restored as the passing behavior.

   **The wake design resurrected behind `halt_timer_delivery`
   (fourth attempt, kernel-visible):** the flag restores the third
   attempt's design behind the manifest: the TSC-deadline arming write
   is **blocked** from the in-kernel LAPIC (no host hrtimer, no
   in-kernel delivery), the deadline is owned by the hold path, and a
   wake thread breaks a halted KVM_RUN near the deadline so the run
   loop can advance the virtual clock to it and queue the delivery
   turn (`virtual_time_jump` + `lapic-timer` delivery,
   replay-admitted). Unlike the third attempt, this probe runs with
   the kernel's own account on the COM2 log: when the guest stalls,
   the retained `firecracker-debug.log` names the exact kernel
   position instead of leaving the re-arm cycle to guesswork. A stale
   wake (a newer arm superseded it) never delivers early — each arm
   carries a generation and only the latest one's wake requests the
   delivery.

   **What the fourth attempt proved (all evidenced in one CI run):**
   the arm record lands (`halt-timer arm:` log), the wake fires
   immediately, the virtual jump positions the tick at the deadline —
   `arm_vtime + (deadline_ticks − tsc_at_arm)` in the guest's own TSC
   arithmetic, the base-mixing bug that first degenerated every tick
   into an immediate delivery — the host turn is recorded
   (`virtual_time_jump:3977034`), the delivery turn queues, and the
   vector is asserted through the LAPIC IRR (KVM_INTERRUPT is
   userspace-irqchip only and returns ENXIO under the in-kernel
   irqchip). **The frontier is settled by the kernel's own account:**
   the probe writes `/proc/interrupts` into `/dev/kmsg` so the kernel
   emits it on the COM2 early console, and the retained log answers
   in one line: `LOC: 0 Local timer interrupts`. The guest never took
   a single LAPIC-timer interrupt - the SET_LAPIC IRR assert is
   written into the restored LAPIC state but KVM's event-injection
   path does not surface it at the guest's interrupt windows. The
   wake/jump/queue chain is correct and evidenced; the missing
   primitive is userspace local-vector injection itself, which the
   in-kernel irqchip does not offer. The userspace APIC (option B) is
   the required path for stages 3-4; until then the flag ships off
   and the write-through arming stays the passing behavior, with the
   arm records (stage 2) and the pairing admission as the shipped,
   green foundation.

   **The candidate resolution — landed:** the write-through arming is
   restored (KVM's hrtimer wakes the halted vCPU — no storm, no hang),
   the arm record is the replay-admitted decision, and the delivery
   admits only against its own arm (`admit_arm_delivery`: the
   immediately preceding stream decision must be the arm that produced
   the delivery). The stream reads `arm → interrupt` pairs at
   deterministic positions; a stale or re-ordered delivery diverges by
   name; any intervening decision clears the pairing. **CI finding:** a
   moved episode now diverges at the delivery admission (named), and
   the hold-replay run completes the workload through the reboot marker
   — the reboot-path interaction resolves itself once the delivery is
   coupled to its arm: the guest's `reboot -f` ran and the reset landed
   under the write-through arming (the replay divergence is the
   *recorded* stream carrying the delivery at its host-timed position,
   which is the stage-4 work: make the delivery position virtual).

   **Debug finding:** raising the guest's kernel loglevel to diagnose
   the hang breaks the workload itself — the kernel console writes
   interleave with the sensor data on the same UART, so the init's
   `read` consumes kernel text instead (`set -e` exits 1). Halt-frontier
   debugging therefore needs a second serial port (or the guest's own
   log mechanism) — one more item for the KVM-host session, alongside
   the reboot-point marker the hold workload now prints
   (`THES:M:reboot` on the serial log pins whether the guest reached
   the reboot syscall).

   **COM2 console wiring (landed):** the COM2 debug serial device (port
   0x2f8, IRQ 3, output-only) is merged behind `debug_serial_out_path`,
   and the test flow wires it end to end: the checkpoint-restore branch
   PUTs the serial config *before* the checkpoint load (so the restored
   VM has the COM2 device), and the COM2 output-complete interrupts are
   admitted as `vcpu:N:interrupt:debug-serial:3` machine-stream records
   (the vmm validator, the offline evidence validator, and the
   checkpoint source mapping all accept the source).

   **Kernel visibility (landed):** plans can override the capture boot
   arguments, and the probe plan steers `console=ttyS1` with the
   loglevel raised and `earlyprintk=serial,0x2f8`. The kernel's own
   account now lands on the retained COM2 log while ttyS0 stays the
   clean data channel. Because kernel printk content depends on real
   host time (the kvm-clock offset, calibration values), the COM2
   port range is a **diagnostic observation channel**: accesses to it
   are handled by the device and visible in the debug log, but never
   recorded into the machine stream and never replay-matched. With
   that exclusion, a hold-mode recording with kernel visibility
   completes its workload end to end (`THES:M:42`, the sensor reading,
   and the reboot marker all land on ttyS0).
   The first probe with visibility is on record: the kernel boots clean
   through `TSC deadline timer available`, runs the whole workload, and
   reaches `reboot: machine restart` **in hold mode** - the
   halt-frontier hang from the earlier probes did not reproduce under
   the shipped write-through arming. The kernel's full account sits in
   the retained COM2 log for every subsequent iteration, and the hold
   replay still fails closed on episode movement (naming the
   `lapic-timer` delivery), exactly as designed.
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
