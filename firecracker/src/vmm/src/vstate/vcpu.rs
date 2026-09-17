// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::{BTreeSet, VecDeque};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Barrier, Condvar, Mutex, Weak};
use std::time::Duration;
use std::{fmt, io, thread};

use kvm_bindings::{KVM_SYSTEM_EVENT_RESET, KVM_SYSTEM_EVENT_SHUTDOWN};
use kvm_ioctls::{VcpuExit, VcpuFd, VmFd};
use libc::{c_int, c_void, siginfo_t};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vmm_sys_util::errno;
use vmm_sys_util::eventfd::EventFd;

use crate::FcExitCode;
pub use crate::arch::{KvmVcpu, KvmVcpuConfigureError, KvmVcpuError, Peripherals, VcpuState};
use crate::cpu_config::templates::{CpuConfiguration, GuestConfigError};
#[cfg(feature = "gdb")]
use crate::gdb::target::{GdbTargetError, get_raw_tid};
use crate::logger::{IncMetric, METRICS, error, info, warn};
use crate::seccomp::{BpfProgram, BpfProgramRef};
use crate::utils::signal::{Killable, register_signal_handler, sigrtmin};
use crate::vstate::bus::Bus;
use crate::vstate::vm::KvmVm;

/// Signal number (SIGRTMIN) used to kick Vcpus.
pub const VCPU_RTSIG_OFFSET: i32 = 0;

/// Maximum time to wait for a vCPU thread to exit when dropping its handle.
const VCPU_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Sample one executed location per this many deterministic, handled exits.
/// This is deliberately far sparser than single-stepping: normal virtio and
/// UART workloads keep their normal device path while still yielding stable
/// execution identities at the exits Theseus controls.
const EXECUTION_LOCATION_SAMPLE_EXITS: u64 = 64;

/// Number of readable decisions retained beside the complete rolling digest.
/// The digest covers every decision; this tail only makes a divergence useful
/// to a human without allowing a long-running guest to consume unbounded RAM.
const EXECUTION_DECISION_TAIL: usize = 32;

/// Bound retained replay input so a guest cannot grow a result without limit.
const MACHINE_EXECUTION_TRACE_LIMIT: usize = 1_048_576;

/// A wrong vCPU waits briefly for the recorded vCPU to reach its pending exit.
/// Expiry fails replay closed instead of deadlocking VM pause or teardown.
const MACHINE_EXECUTION_TURN_TIMEOUT: Duration = Duration::from_secs(1);

/// Portable evidence for the exact ordered KVM exits handled by one vCPU.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ExecutionLedgerEvidence {
    /// Number of decisions covered by `sha256`.
    pub decisions: u64,
    /// SHA-256 of length-framed decision records in execution order.
    pub sha256: String,
    /// Last bounded decisions, in order, for diagnosis.
    pub tail: Vec<String>,
}

/// Runtime-owned rolling state. A campaign branch clones this state together
/// with its VM checkpoint, so child suffixes continue the exact parent ledger.
#[derive(Clone, Debug)]
pub struct ExecutionLedger {
    hasher: Sha256,
    decisions: u64,
    tail: VecDeque<String>,
}

impl Default for ExecutionLedger {
    fn default() -> Self {
        Self {
            hasher: Sha256::new(),
            decisions: 0,
            tail: VecDeque::with_capacity(EXECUTION_DECISION_TAIL),
        }
    }
}

impl ExecutionLedger {
    fn record(&mut self, decision: String) {
        self.hasher.update(
            u64::try_from(decision.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        self.hasher.update(decision.as_bytes());
        self.decisions = self.decisions.saturating_add(1);
        if self.tail.len() == EXECUTION_DECISION_TAIL {
            self.tail.pop_front();
        }
        self.tail.push_back(decision);
    }

    /// Return stable, portable evidence without consuming the rolling state.
    pub fn evidence(&self) -> ExecutionLedgerEvidence {
        ExecutionLedgerEvidence {
            decisions: self.decisions,
            sha256: format!("{:x}", self.hasher.clone().finalize()),
            tail: self.tail.iter().cloned().collect(),
        }
    }
}

/// Branch-owned machine execution state. Unlike portable evidence, this keeps
/// every bounded decision needed to actively enforce a later replay.
#[derive(Clone, Debug, Default)]
pub struct MachineExecutionState {
    ledger: ExecutionLedger,
    trace: Vec<String>,
    pending_interrupts: VecDeque<PendingInterrupt>,
}

impl MachineExecutionState {
    /// Portable digest and tail for reports and certificates.
    pub fn ledger_evidence(&self) -> ExecutionLedgerEvidence {
        self.ledger.evidence()
    }

    /// Exact retained decisions used to drive replay.
    pub fn trace(&self) -> &[String] {
        &self.trace
    }

    /// Serialize the bounded prefix and undelivered requests, not hash internals.
    pub fn checkpoint_state(&self) -> CheckpointExecutionState {
        CheckpointExecutionState {
            trace: self.trace.clone(),
            pending_interrupts: self.pending_interrupts.iter().map(|request| CheckpointInterrupt {
                source: request.source.to_owned(), gsi: request.gsi, coalesce: request.coalesce,
            }).collect(),
        }
    }
}

/// Portable state from which all execution hashes are independently rebuilt.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointExecutionState {
    /// Complete inherited decisions.
    pub trace: Vec<String>,
    /// Ordered requests that have not yet reached KVM.
    pub pending_interrupts: Vec<CheckpointInterrupt>,
}

/// A retained userspace-device interrupt request.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointInterrupt {
    /// Stable supported device source.
    pub source: String,
    /// Guest system interrupt number.
    pub gsi: u32,
    /// Whether repeated level requests coalesce.
    pub coalesce: bool,
}

impl CheckpointExecutionState {
    /// Validate actors and queues, then recompute local and machine hash state.
    pub fn restore(&self, vcpu_count: usize) -> Result<(MachineExecutionState, Vec<ExecutionLedger>), String> {
        MachineExecutionController::validate_trace(&self.trace)?;
        if vcpu_count == 0 || vcpu_count > 255 || self.pending_interrupts.len() > MACHINE_EXECUTION_TRACE_LIMIT {
            return Err("checkpoint has an invalid vCPU or pending-interrupt count".into());
        }
        let mut machine = MachineExecutionState::default();
        let mut local = vec![ExecutionLedger::default(); vcpu_count];
        for record in &self.trace {
            if let Some(MachineExecutionActor::Vcpu(id)) = machine_record_actor(record) {
                let ledger = local.get_mut(usize::from(id)).ok_or("checkpoint trace names an absent vCPU")?;
                ledger.record(record.splitn(3, ':').nth(2).expect("validated decision").to_owned());
            }
            machine.ledger.record(record.clone());
        }
        machine.trace = self.trace.clone();
        for request in &self.pending_interrupts {
            let source = match request.source.as_str() {
                "serial" => "serial", "i8042" => "i8042", "virtio-mmio" => "virtio-mmio",
                "virtio-msix" => "virtio-msix", "vmgenid" => "vmgenid", "vmclock" => "vmclock",
                _ => return Err("checkpoint contains an unsupported interrupt source".into()),
            };
            if request.gsi >= 1024 {
                return Err("checkpoint interrupt is outside the supported GSI range".into());
            }
            machine.pending_interrupts.push_back(PendingInterrupt { source, gsi: request.gsi, coalesce: request.coalesce });
        }
        Ok((machine, local))
    }
}

#[derive(Debug)]
struct MachineExecutionControl {
    execution: MachineExecutionState,
    expected: Option<Vec<String>>,
    position: usize,
    divergence: Option<String>,
}

/// Shared gate that serializes device effects, explicit host inputs, and
/// controlled interrupt injection and, during replay, admits only the actor
/// named by the next retained decision.
#[derive(Debug)]
pub struct MachineExecutionController {
    state: Mutex<MachineExecutionControl>,
    pending_interrupts: Mutex<VecDeque<PendingInterrupt>>,
    deterministic_interrupts: AtomicBool,
    control_requested: AtomicBool,
    shutdown_requested: AtomicBool,
    interrupt_kickers: Mutex<Vec<Weak<Mutex<Option<VcpuInterruptKick>>>>>,
    turn_changed: Condvar,
}

impl Default for MachineExecutionController {
    fn default() -> Self {
        Self {
            state: Mutex::new(MachineExecutionControl {
                execution: MachineExecutionState::default(),
                expected: None,
                position: 0,
                divergence: None,
            }),
            pending_interrupts: Mutex::new(VecDeque::new()),
            deterministic_interrupts: AtomicBool::new(false),
            control_requested: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(false),
            interrupt_kickers: Mutex::new(Vec::new()),
            turn_changed: Condvar::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingInterrupt {
    source: &'static str,
    gsi: u32,
    coalesce: bool,
}

/// A separate KVM mapping lets device threads wake a vCPU without taking the
/// handle lock held by pause, snapshot, and query operations.
#[derive(Debug)]
struct VcpuInterruptKick {
    vcpu_fd: VcpuFd,
    // musl models pthread_t as a pointer, glibc as an integer. Keep only its
    // opaque identity; this code never dereferences it.
    pthread: usize,
    ready: Arc<AtomicBool>,
}

impl VcpuInterruptKick {
    fn kick(&mut self) -> io::Result<()> {
        // Requests queued during startup are consumed at the first guest
        // entry. Do not signal a thread before it installs the kick handler.
        if !self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        self.vcpu_fd.set_kvm_immediate_exit(1);
        fence(Ordering::Release);
        // SAFETY: VcpuHandle clears this object under the same mutex before
        // joining the thread, so its pthread handle cannot be reused here.
        let result = unsafe {
            libc::pthread_kill(self.pthread as libc::pthread_t, sigrtmin() + VCPU_RTSIG_OFFSET)
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }
}

/// A device interrupt request routed through the deterministic machine stream.
#[derive(Clone, Debug)]
pub(crate) struct DeferredInterrupt {
    controller: Weak<MachineExecutionController>,
    source: &'static str,
    gsi: u32,
    coalesce: bool,
}

impl DeferredInterrupt {
    pub(crate) fn level(
        controller: &Arc<MachineExecutionController>,
        source: &'static str,
        gsi: u32,
    ) -> Self {
        Self {
            controller: Arc::downgrade(controller),
            source,
            gsi,
            coalesce: true,
        }
    }

    pub(crate) fn edge(
        controller: &Arc<MachineExecutionController>,
        source: &'static str,
        gsi: u32,
    ) -> Self {
        Self {
            controller: Arc::downgrade(controller),
            source,
            gsi,
            coalesce: false,
        }
    }

    pub(crate) fn trigger(&self) -> io::Result<()> {
        self.controller
            .upgrade()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "interrupt controller was dropped",
                )
            })?
            .request_interrupt(self.source, self.gsi, self.coalesce)
    }
}

impl MachineExecutionController {
    fn request_vcpu_control(&self) {
        self.control_requested.store(true, Ordering::Release);
        self.turn_changed.notify_all();
    }

    fn resume_vcpu_execution(&self) {
        self.control_requested.store(false, Ordering::Release);
        self.turn_changed.notify_all();
    }

    fn vcpu_control_requested(&self) -> bool {
        self.control_requested.load(Ordering::Acquire)
    }

    /// Freeze device-effect admission after a guest terminal request.
    pub(crate) fn request_shutdown(&self) {
        if self.shutdown_requested.swap(true, Ordering::AcqRel) {
            return;
        }
        self.turn_changed.notify_all();
        if let Err(error) = self.kick_vcpus() {
            error!("Failed to kick vCPUs at guest shutdown: {error}");
        }
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    /// Stop admission only after the current serialized effect has committed.
    pub(crate) fn freeze_admission(&self) {
        let _state = self.state.lock().expect("machine execution controller lock poisoned");
        self.request_shutdown();
    }

    fn execution_state(&self) -> MachineExecutionState {
        let mut execution = self
            .state
            .lock()
            .expect("machine execution controller lock poisoned")
            .execution
            .clone();
        execution.pending_interrupts = self
            .pending_interrupts
            .lock()
            .expect("pending interrupt queue lock poisoned")
            .clone();
        execution
    }

    fn restore_execution_state(&self, mut execution: MachineExecutionState) {
        let pending_interrupts = std::mem::take(&mut execution.pending_interrupts);
        let mut state = self
            .state
            .lock()
            .expect("machine execution controller lock poisoned");
        state.position = execution.trace.len();
        state.execution = execution;
        state.expected = None;
        state.divergence = None;
        let mut live_interrupts = self
            .pending_interrupts
            .lock()
            .expect("pending interrupt queue lock poisoned");
        let mut restored_interrupts = pending_interrupts;
        for interrupt in live_interrupts.drain(..) {
            if !interrupt.coalesce || !restored_interrupts.contains(&interrupt) {
                restored_interrupts.push_back(interrupt);
            }
        }
        *live_interrupts = restored_interrupts;
        drop(live_interrupts);
        self.turn_changed.notify_all();
    }

    pub(crate) fn enable_deterministic_interrupts(&self) {
        self.deterministic_interrupts.store(true, Ordering::Release);
    }

    fn attach_interrupt_kicker(&self, kicker: Weak<Mutex<Option<VcpuInterruptKick>>>) {
        self.interrupt_kickers
            .lock()
            .expect("vCPU interrupt kicker list lock poisoned")
            .push(kicker);
    }

    pub(crate) fn deterministic_interrupts_enabled(&self) -> bool {
        self.deterministic_interrupts.load(Ordering::Acquire)
    }

    pub(crate) fn request_edge_interrupt(&self, source: &'static str, gsi: u32) -> io::Result<()> {
        self.request_interrupt(source, gsi, false)
    }

    fn request_interrupt(&self, source: &'static str, gsi: u32, coalesce: bool) -> io::Result<()> {
        let interrupt = PendingInterrupt {
            source,
            gsi,
            coalesce,
        };
        let mut pending = self
            .pending_interrupts
            .lock()
            .expect("pending interrupt queue lock poisoned");
        // Level sources remain asserted until the guest clears their status;
        // repeated triggers before injection are not new line transitions.
        let enqueued = !coalesce || !pending.contains(&interrupt);
        if enqueued {
            pending.push_back(interrupt);
        }
        drop(pending);
        if !enqueued {
            return Ok(());
        }
        self.turn_changed.notify_all();
        self.kick_vcpus()
    }

    fn kick_vcpus(&self) -> io::Result<()> {
        let mut kickers = self
            .interrupt_kickers
            .lock()
            .expect("vCPU interrupt kicker list lock poisoned");
        kickers.retain(|kicker| kicker.strong_count() > 0);
        for kicker in kickers.iter().filter_map(Weak::upgrade) {
            if let Some(kicker) = kicker
                .lock()
                .expect("vCPU interrupt kicker lock poisoned")
                .as_mut()
            {
                kicker.kick()?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pending_interrupts_for_test(&self) -> Vec<(&'static str, u32)> {
        self.pending_interrupts
            .lock()
            .expect("pending interrupt queue lock poisoned")
            .iter()
            .map(|interrupt| (interrupt.source, interrupt.gsi))
            .collect()
    }

    fn deliver_pending_interrupt(
        &self,
        vcpu: u8,
        vm_fd: &VmFd,
        ledger: &Arc<Mutex<ExecutionLedger>>,
    ) -> Result<bool, String> {
        self.deliver_pending_interrupts_with(vcpu, ledger, |gsi| {
            vm_fd
                .set_irq_line(gsi, true)
                .and_then(|()| vm_fd.set_irq_line(gsi, false))
                .map_err(|error| error.to_string())
        })
    }

    fn deliver_pending_interrupts_with(
        &self,
        vcpu: u8,
        ledger: &Arc<Mutex<ExecutionLedger>>,
        mut inject: impl FnMut(u32) -> Result<(), String>,
    ) -> Result<bool, String> {
        let mut delivered_any = false;
        loop {
            let delivered =
                self.deliver_pending_interrupt_with(vcpu, ledger, |gsi| inject(gsi))?;
            delivered_any |= delivered;
            if !delivered || !self.may_deliver_another_interrupt(vcpu) {
                return Ok(delivered_any);
            }
        }
    }

    fn may_deliver_another_interrupt(&self, vcpu: u8) -> bool {
        let state = self
            .state
            .lock()
            .expect("machine execution controller lock poisoned");
        match state.expected.as_ref() {
            None => true,
            Some(expected) => expected
                .get(state.position)
                .is_some_and(|record| record.starts_with(&format!("vcpu:{vcpu}:interrupt:"))),
        }
    }

    fn deliver_pending_interrupt_with(
        &self,
        vcpu: u8,
        ledger: &Arc<Mutex<ExecutionLedger>>,
        inject: impl FnOnce(u32) -> Result<(), String>,
    ) -> Result<bool, String> {
        if !self.deterministic_interrupts_enabled() {
            return Ok(false);
        }
        let mut state = self
            .state
            .lock()
            .expect("machine execution controller lock poisoned");
        let (mut pending, pending_index) = loop {
            if self.shutdown_requested() || self.vcpu_control_requested() {
                return Ok(false);
            }
            if let Some(detail) = state.divergence.clone() {
                return Err(detail);
            }
            let pending = self
                .pending_interrupts
                .lock()
                .expect("pending interrupt queue lock poisoned");
            let Some(_) = pending.front() else {
                if state
                    .expected
                    .as_ref()
                    .and_then(|expected| expected.get(state.position))
                    .is_some_and(|record| record.starts_with(&format!("vcpu:{vcpu}:interrupt:")))
                {
                    // The recorded device completion can arrive on the host
                    // event loop after this vCPU reaches its injection turn.
                    // Wait for that producer without running more guest code.
                    drop(pending);
                    state = self
                        .turn_changed
                        .wait(state)
                        .expect(
                            "machine execution controller lock poisoned while waiting for interrupt request",
                        );
                    continue;
                }
                return Ok(false);
            };
            let Some(expected) = state.expected.as_ref() else {
                break (pending, 0);
            };
            let Some(expected_record) = expected.get(state.position) else {
                // Exact replay ends at a control-plane cut. Leave later
                // interrupts queued so the vCPU can return to its control
                // loop and be paused without extending the retained stream.
                return Ok(false);
            };
            if expected_record.starts_with(&format!("vcpu:{vcpu}:interrupt:")) {
                if let Some(index) = pending.iter().position(|interrupt| {
                    format!(
                        "vcpu:{vcpu}:interrupt:{}:{}",
                        interrupt.source, interrupt.gsi
                    ) == *expected_record
                }) {
                    break (pending, index);
                }
                // Async sources may publish in a different host order after
                // checkpoint restore. Keep unrelated requests queued and
                // wait for the exact interrupt selected by the replay stream.
                drop(pending);
                state = self
                    .turn_changed
                    .wait(state)
                    .expect(
                        "machine execution controller lock poisoned while waiting for recorded interrupt",
                    );
                continue;
            }
            if machine_record_actor(expected_record) == Some(MachineExecutionActor::Vcpu(vcpu)) {
                // A pending interrupt may have arrived before an earlier
                // recorded device access by this same vCPU. Let KVM produce
                // that effect first; exact interrupt identity is the turn,
                // not merely vCPU ownership.
                return Ok(false);
            }
            drop(pending);
            let (next, timeout) = self
                .turn_changed
                .wait_timeout(state, MACHINE_EXECUTION_TURN_TIMEOUT)
                .expect(
                    "machine execution controller lock poisoned while waiting to deliver interrupt",
                );
            state = next;
            if timeout.timed_out() {
                let detail = format!(
                    "machine execution replay could not deliver a pending interrupt at decision {} because another actor was expected",
                    state.position
                );
                state.divergence = Some(detail.clone());
                self.turn_changed.notify_all();
                return Err(detail);
            }
        };
        let interrupt = pending
            .get(pending_index)
            .expect("pending interrupt disappeared");
        let decision = format!("interrupt:{}:{}", interrupt.source, interrupt.gsi);
        let record = format!("vcpu:{vcpu}:{decision}");
        if let Some(expected) = state.expected.as_ref() {
            let expected_record = expected.get(state.position);
            if expected_record != Some(&record) {
                let detail = format!(
                    "machine execution replay diverged at decision {}: expected {:?}, observed {record:?}",
                    state.position, expected_record
                );
                state.divergence = Some(detail.clone());
                self.turn_changed.notify_all();
                return Err(detail);
            }
        }
        if state.execution.trace.len() == MACHINE_EXECUTION_TRACE_LIMIT {
            let detail = format!(
                "machine execution trace exceeded {MACHINE_EXECUTION_TRACE_LIMIT} decisions"
            );
            state.divergence = Some(detail.clone());
            self.turn_changed.notify_all();
            return Err(detail);
        }
        inject(interrupt.gsi)
            .map_err(|error| format!("failed to deliver {decision}: {error}"))?;
        pending.remove(pending_index);
        drop(pending);
        ledger
            .lock()
            .expect("execution ledger lock poisoned")
            .record(decision);
        state.execution.ledger.record(record.clone());
        state.execution.trace.push(record);
        state.position = state.position.saturating_add(1);
        self.turn_changed.notify_all();
        Ok(true)
    }

    pub(crate) fn validate_trace(expected: &[String]) -> Result<(), String> {
        if expected.len() > MACHINE_EXECUTION_TRACE_LIMIT {
            return Err(format!(
                "machine execution trace has {} decisions; limit is {MACHINE_EXECUTION_TRACE_LIMIT}",
                expected.len()
            ));
        }
        if expected
            .iter()
            .any(|record| machine_record_actor(record).is_none())
        {
            return Err("machine execution trace contains a malformed decision".to_owned());
        }
        Ok(())
    }

    fn enforce(&self, expected: Vec<String>) -> Result<(), String> {
        Self::validate_trace(&expected)?;
        let mut state = self
            .state
            .lock()
            .expect("machine execution controller lock poisoned");
        if !expected.starts_with(&state.execution.trace) {
            return Err(format!(
                "machine execution trace diverged before replay began at decision {}",
                state
                    .execution
                    .trace
                    .iter()
                    .zip(&expected)
                    .take_while(|(actual, expected)| actual == expected)
                    .count()
            ));
        }
        state.position = state.execution.trace.len();
        state.expected = Some(expected);
        state.divergence = None;
        self.turn_changed.notify_all();
        Ok(())
    }

    fn replay_divergence(&self) -> Option<String> {
        self.state
            .lock()
            .expect("machine execution controller lock poisoned")
            .divergence.clone()
    }

    fn replay_error(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .expect("machine execution controller lock poisoned");
        state.divergence.clone().or_else(|| {
            state.expected.as_ref().and_then(|expected| {
                (state.position != expected.len()).then(|| {
                    format!(
                        "machine execution replay stopped at decision {} of {}",
                        state.position,
                        expected.len()
                    )
                })
            })
        })
    }

    fn wait_for_replay_progress(
        &self,
        position: usize,
        timeout: Duration,
    ) -> Result<bool, String> {
        let state = self
            .state
            .lock()
            .expect("machine execution controller lock poisoned");
        if state.expected.is_none() || state.execution.trace.len() > position {
            return Ok(true);
        }
        if let Some(detail) = state.divergence.clone() {
            return Err(detail);
        }
        let (state, _) = self
            .turn_changed
            .wait_timeout_while(state, timeout, |state| {
                state.expected.is_some()
                    && state.divergence.is_none()
                    && state.execution.trace.len() <= position
            })
            .expect("machine execution controller lock poisoned while waiting for replay progress");
        if let Some(detail) = state.divergence.clone() {
            return Err(detail);
        }
        let progressed = state.expected.is_none() || state.execution.trace.len() > position;
        if !progressed
            && state.position == position
            && let Some((_, source, gsi)) = state
                .expected
                .as_ref()
                .and_then(|expected| expected.get(position))
                .and_then(|record| machine_interrupt_record(record))
        {
            let mut pending = self
                .pending_interrupts
                .lock()
                .expect("pending interrupt queue lock poisoned");
            let interrupt = PendingInterrupt {
                source,
                gsi,
                coalesce: true,
            };
            if !pending.contains(&interrupt) {
                // Interrupt delivery is itself a recorded replay decision.
                // Materialize that turn after the real source had a chance to
                // publish, then let later device accesses verify its state.
                pending.push_back(interrupt);
                self.turn_changed.notify_all();
            }
        }
        Ok(progressed)
    }

    pub(crate) fn apply_host_effect<T, E>(
        &self,
        effect: String,
        apply: impl FnOnce() -> Result<T, E>,
    ) -> Result<Result<T, E>, String> {
        if !valid_machine_host_effect(&effect) {
            return Err("machine execution host effect is malformed or unsupported".to_owned());
        }
        let record = format!("host:{effect}");
        let mut state = self
            .state
            .lock()
            .expect("machine execution controller lock poisoned");
        loop {
            if let Some(detail) = state.divergence.clone() {
                return Err(detail);
            }
            if self.shutdown_requested() {
                return Err("machine execution rejected host input after guest shutdown".to_owned());
            }
            let Some(expected) = state.expected.as_ref() else {
                break;
            };
            let Some(expected_record) = expected.get(state.position) else {
                break;
            };
            if machine_record_actor(expected_record) == Some(MachineExecutionActor::Host) {
                break;
            }
            let (next, timeout) = self
                .turn_changed
                .wait_timeout(state, MACHINE_EXECUTION_TURN_TIMEOUT)
                .expect("machine execution controller lock poisoned while waiting for host turn");
            state = next;
            if timeout.timed_out() {
                let detail = format!(
                    "machine execution replay expected a vCPU effect at decision {}, but host effect {effect:?} arrived",
                    state.position
                );
                state.divergence = Some(detail.clone());
                self.turn_changed.notify_all();
                return Err(detail);
            }
        }
        if state.execution.trace.len() == MACHINE_EXECUTION_TRACE_LIMIT {
            let detail = format!(
                "machine execution trace exceeded {MACHINE_EXECUTION_TRACE_LIMIT} decisions"
            );
            state.divergence = Some(detail.clone());
            self.turn_changed.notify_all();
            return Err(detail);
        }
        if let Some(expected) = state.expected.as_ref() {
            if expected.get(state.position) != Some(&record) {
                let detail = format!(
                    "machine execution replay diverged at decision {}: expected {:?}, observed {record:?}",
                    state.position,
                    expected.get(state.position)
                );
                state.divergence = Some(detail.clone());
                self.turn_changed.notify_all();
                return Err(detail);
            }
        }
        let result = apply();
        if result.is_err() {
            return Ok(result);
        }
        state.execution.ledger.record(record.clone());
        state.execution.trace.push(record);
        state.position = state.position.saturating_add(1);
        self.turn_changed.notify_all();
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MachineExecutionActor {
    Vcpu(u8),
    Host,
}

fn machine_record_actor(record: &str) -> Option<MachineExecutionActor> {
    if let Some(effect) = record.strip_prefix("host:") {
        return valid_machine_host_effect(effect).then_some(MachineExecutionActor::Host);
    }
    let (vcpu, effect) = record.strip_prefix("vcpu:")?.split_once(':')?;
    if !valid_machine_vcpu_effect(effect) {
        return None;
    }
    let id = vcpu.parse::<u8>().ok()?;
    (vcpu == id.to_string()).then_some(MachineExecutionActor::Vcpu(id))
}

fn machine_interrupt_record(record: &str) -> Option<(u8, &'static str, u32)> {
    let (vcpu, effect) = record.strip_prefix("vcpu:")?.split_once(':')?;
    let id = vcpu.parse::<u8>().ok()?;
    if vcpu != id.to_string() {
        return None;
    }
    let interrupt = effect.strip_prefix("interrupt:")?;
    let (source, gsi) = interrupt.split_once(':')?;
    let source = match source {
        "serial" => "serial",
        "i8042" => "i8042",
        "virtio-mmio" => "virtio-mmio",
        "virtio-msix" => "virtio-msix",
        "vmgenid" => "vmgenid",
        "vmclock" => "vmclock",
        _ => return None,
    };
    Some((id, source, gsi.parse().ok()?))
}

fn valid_machine_vcpu_effect(effect: &str) -> bool {
    let Some(interrupt) = effect.strip_prefix("interrupt:") else {
        return !effect.is_empty();
    };
    let Some((source, gsi_text)) = interrupt.split_once(':') else {
        return false;
    };
    let Ok(gsi) = gsi_text.parse::<u32>() else {
        return false;
    };
    matches!(
        source,
        "serial" | "virtio-mmio" | "virtio-msix" | "vmgenid" | "vmclock" | "i8042"
    ) && gsi_text == gsi.to_string()
}

fn valid_machine_host_effect(effect: &str) -> bool {
    if effect == "ctrl_alt_del" {
        return true;
    }
    if let Some(byte) = effect.strip_prefix("control_event:") {
        return valid_lowercase_hex(byte) && byte.len() == 2;
    }
    if let Some(serial) = effect.strip_prefix("serial_input:") {
        let Some((length_text, bytes)) = serial.split_once(':') else {
            return false;
        };
        let Ok(length) = length_text.parse::<usize>() else {
            return false;
        };
        return length > 0
            && length_text == length.to_string()
            && bytes.len() == length.saturating_mul(2)
            && valid_lowercase_hex(bytes);
    }
    let Some(delta_text) = effect.strip_prefix("virtual_time_jump:") else {
        return false;
    };
    delta_text
        .parse::<u64>()
        .is_ok_and(|delta| delta > 0 && delta_text == delta.to_string())
}

fn valid_lowercase_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|digit| digit.is_ascii_hexdigit() && !digit.is_ascii_uppercase())
}

/// Errors associated with the wrappers over KVM ioctls.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VcpuError {
    /// Error creating vcpu config: {0}
    VcpuConfig(GuestConfigError),
    /// Received error signaling kvm exit: {0}
    FaultyKvmExit(String),
    /// Failed to signal vcpu: {0}
    SignalVcpu(vmm_sys_util::errno::Error),
    /// Unexpected kvm exit received: {0}
    UnhandledKvmExit(String),
    /// Failed to run action on vcpu: {0}
    VcpuResponse(KvmVcpuError),
    /// Cannot spawn a new vCPU thread: {0}
    VcpuSpawn(io::Error),
    /// Vcpu not present in TLS
    VcpuTlsNotPresent,
    /// Error with gdb request sent
    #[cfg(feature = "gdb")]
    GdbRequest(GdbTargetError),
}

/// Encapsulates configuration parameters for the guest vCPUS.
#[derive(Debug)]
pub struct VcpuConfig {
    /// Number of guest VCPUs.
    pub vcpu_count: u8,
    /// Enable simultaneous multithreading in the CPUID configuration.
    pub smt: bool,
    /// Configuration for vCPU
    pub cpu_config: CpuConfiguration,
}

/// Error type for [`Vcpu::start_threaded`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum StartThreadedError {
    /// Failed to spawn vCPU thread: {0}
    Spawn(std::io::Error),
    /// Failed to clone kvm Vcpu fd: {0}
    CopyFd(CopyKvmFdError),
}

/// Error type for [`Vcpu::copy_kvm_vcpu_fd`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum CopyKvmFdError {
    /// Error with libc dup of kvm Vcpu fd
    DupError(#[from] std::io::Error),
    /// Error creating the Vcpu from the duplicated Vcpu fd
    CreateVcpuError(#[from] kvm_ioctls::Error),
}

/// A wrapper around creating and using a vcpu.
#[derive(Debug)]
pub struct Vcpu {
    /// Access to kvm-arch specific functionality.
    pub kvm_vcpu: KvmVcpu,

    /// File descriptor for vcpu to trigger exit event on vmm.
    exit_evt: EventFd,
    /// Shared VM descriptor used for synchronous deterministic IRQ delivery.
    vm_fd: Arc<VmFd>,
    /// Debugger emitter for gdb events
    #[cfg(feature = "gdb")]
    gdb_event: Option<Sender<usize>>,
    /// The receiving end of events channel owned by the vcpu side.
    event_receiver: Receiver<VcpuEvent>,
    /// The transmitting end of the events channel which will be given to the handler.
    event_sender: Option<Sender<VcpuEvent>>,
    /// The receiving end of the responses channel which will be given to the handler.
    response_receiver: Option<Receiver<VcpuResponse>>,
    /// The transmitting end of the responses channel owned by the vcpu side.
    response_sender: Sender<VcpuResponse>,

    /// Theseus (Track B′): the tick-stepped virtual clock, when enabled.
    vclock: Option<crate::vstate::vclock::VirtualClock>,
    /// Guest-visible exits elapsed in the current quantum.
    exits_since_tick: u64,
    /// Exits per quantum (tick boundary).
    exits_per_tick: u64,
    /// Whether the guest clock has been anchored to virtual time zero.
    /// aarch64 can only write the counter offset after KVM_ARM_VCPU_INIT
    /// (vCPU configure time), so the anchor is applied on first run.
    vclock_anchored: bool,
    /// Stable guest PCs sampled from the vCPU thread at handled KVM exits.
    execution_locations: Arc<Mutex<BTreeSet<u64>>>,
    execution_location_exits: u64,
    /// Exact ordered ledger of guest-visible KVM exits handled for this vCPU.
    execution_ledger: Arc<Mutex<ExecutionLedger>>,
    /// VM-wide ledger shared by every vCPU. Holding this lock while handling
    /// an exit gives device effects and host inputs one explicit total order.
    machine_execution: Arc<MachineExecutionController>,
}

/// States of the vCPU thread's run loop.
#[derive(Debug)]
enum VcpuRunState {
    /// The vCPU is executing guest code via `KVM_RUN`.
    Running,
    /// The vCPU is paused, waiting for events.
    Paused,
    /// The vCPU thread's run loop has finished; the thread will exit.
    Finished,
}

impl Vcpu {
    /// Registers a signal handler which kicks the vcpu running on the current thread, if there is
    /// one.
    fn register_kick_signal_handler(&mut self) {
        extern "C" fn handle_signal(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
            // We write to the immediate_exit from other thread, so make sure the read in the
            // KVM_RUN sees the up to date value
            fence(Ordering::Acquire);
        }
        register_signal_handler(sigrtmin() + VCPU_RTSIG_OFFSET, handle_signal)
            .expect("Failed to register vcpu signal handler");
    }

    /// Constructs a new VCPU for `vm`.
    ///
    /// # Arguments
    ///
    /// * `index` - Represents the 0-based CPU index between [0, max vcpus).
    /// * `vm` - The vm to which this vcpu will get attached.
    /// * `exit_evt` - An `EventFd` that will be written into when this vcpu exits.
    pub fn new(
        index: u8,
        vm: &KvmVm,
        exit_evt: EventFd,
        machine_execution: Arc<MachineExecutionController>,
    ) -> Result<Self, VcpuError> {
        let (event_sender, event_receiver) = channel();
        let (response_sender, response_receiver) = channel();
        let kvm_vcpu = KvmVcpu::new(index, vm).unwrap();

        Ok(Vcpu {
            exit_evt,
            vm_fd: Arc::clone(&vm.common.fd),
            event_receiver,
            event_sender: Some(event_sender),
            response_receiver: Some(response_receiver),
            response_sender,
            #[cfg(feature = "gdb")]
            gdb_event: None,
            kvm_vcpu,
            vclock: None,
            exits_since_tick: 0,
            exits_per_tick: crate::vmm_config::machine_config::DEFAULT_EXITS_PER_TICK,
            vclock_anchored: false,
            execution_locations: Arc::new(Mutex::new(BTreeSet::new())),
            execution_location_exits: 0,
            execution_ledger: Arc::new(Mutex::new(ExecutionLedger::default())),
            machine_execution,
        })
    }

    /// Theseus (Track B′): enable tick-stepped virtual time. `tick_ns` of
    /// virtual time is applied per `exits_per_tick` guest-visible exits.
    ///
    /// Quanta are exit-counted, not host-timed: every event the guest can
    /// observe flows through exits we handle, so tick boundaries land
    /// identically on every replay of the same execution prefix.
    pub fn enable_virtual_time(&mut self, tick_ns: u64, exits_per_tick: u64) {
        self.vclock = Some(crate::vstate::vclock::VirtualClock::new(tick_ns));
        self.exits_per_tick = exits_per_tick.max(1);
        self.exits_since_tick = 0;
        self.vclock_anchored = false;
    }

    /// Current virtual-clock state, if enabled.
    pub fn vclock(&self) -> Option<&crate::vstate::vclock::VirtualClock> {
        self.vclock.as_ref()
    }

    /// Restore virtual-clock bookkeeping from a snapshotted vCPU state.
    /// (The guest-visible clock itself — TSC MSR and kvmclock — is restored
    /// through the regular KVM state paths.)
    ///
    /// Callers must match on `VcpuState.vclock` themselves: `Some` → this
    /// method; `None` → [`Vcpu::enable_virtual_time`] if the feature is
    /// configured. No silent defaults.
    pub fn restore_virtual_time(
        &mut self,
        vc_state: &crate::vstate::vclock::VirtualClockState,
        exits: u64,
        exits_per_tick: u64,
    ) {
        self.vclock = Some(crate::vstate::vclock::VirtualClock::restore(vc_state));
        self.exits_per_tick = exits_per_tick.max(1);
        self.exits_since_tick = exits;
        self.vclock_anchored = false;
    }

    fn jump_virtual_time(&mut self, delta_ns: u64) -> Result<(), VcpuError> {
        let clock = self.vclock.as_mut().ok_or_else(|| {
            VcpuError::FaultyKvmExit("virtual time is not enabled for this vCPU".to_owned())
        })?;
        clock.jump(delta_ns);
        self.kvm_vcpu
            .apply_virtual_time(clock.now_ns())
            .map_err(VcpuError::VcpuResponse)?;
        self.vclock_anchored = true;
        Ok(())
    }

    fn virtual_time_ns(&self) -> Option<u64> {
        self.vclock.as_ref().map(|clock| clock.now_ns())
    }

    /// Advance the quantum counter; at each boundary, step the guest's clock.
    ///
    /// Called from the vCPU run loop between `KVM_RUN` invocations — the only
    /// moment where writing the guest clock is race-free.
    #[inline]
    fn maybe_tick(&mut self) {
        self.execution_location_exits = self.execution_location_exits.saturating_add(1);
        if self.execution_location_exits % EXECUTION_LOCATION_SAMPLE_EXITS == 0 {
            self.record_execution_location();
        }
        if self.vclock.is_none() {
            return;
        }
        self.exits_since_tick += 1;
        if self.exits_since_tick < self.exits_per_tick {
            return;
        }
        self.exits_since_tick = 0;

        // Safe: checked above.
        let vclock = self.vclock.as_mut().unwrap();
        vclock.advance();

        // Arch-specific application: TSC write on x86_64, counter offset on
        // aarch64. Both between KVM_RUN invocations.
        if let Err(err) = self.kvm_vcpu.apply_virtual_time(vclock.now_ns()) {
            error!("Failed to apply virtual time: {err:?}");
            METRICS.vcpu.failures.inc();
        }
    }

    fn record_execution_location(&self) {
        #[cfg(target_arch = "aarch64")]
        let location = {
            let mut value = [0u8; 8];
            self.kvm_vcpu
                .fd
                .get_one_reg(crate::arch::aarch64::regs::PC, &mut value)
                .map(|_| u64::from_ne_bytes(value))
        };
        #[cfg(target_arch = "x86_64")]
        let location = self.kvm_vcpu.fd.get_regs().map(|registers| registers.rip);
        if let Ok(location) = location {
            self.execution_locations
                .lock()
                .expect("execution coverage lock poisoned")
                .insert(location);
        }
    }

    /// Sets a MMIO bus for this vcpu.
    pub fn set_mmio_bus(&mut self, mmio_bus: Arc<Bus>) {
        self.kvm_vcpu.peripherals.mmio_bus = Some(mmio_bus);
    }

    /// Attaches the fields required for debugging
    #[cfg(feature = "gdb")]
    pub fn attach_debug_info(&mut self, gdb_event: Sender<usize>) {
        self.gdb_event = Some(gdb_event);
    }

    /// Obtains a copy of the VcpuFd
    pub fn copy_kvm_vcpu_fd(&self, vm: &KvmVm) -> Result<VcpuFd, CopyKvmFdError> {
        // SAFETY: We own this fd so it is considered safe to clone
        let r = unsafe { libc::dup(self.kvm_vcpu.fd.as_raw_fd()) };
        if r < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: We assert this is a valid fd by checking the result from the dup
        unsafe { Ok(vm.fd().create_vcpu_from_rawfd(r)?) }
    }

    /// Moves the vcpu to its own thread and constructs a VcpuHandle.
    /// The handle can be used to control the remote vcpu.
    pub fn start_threaded(
        mut self,
        vm: &KvmVm,
        seccomp_filter: Arc<BpfProgram>,
        barrier: Arc<Barrier>,
    ) -> Result<VcpuHandle, StartThreadedError> {
        let event_sender = self.event_sender.take().expect("vCPU already started");
        let response_receiver = self.response_receiver.take().unwrap();
        let vcpu_fd = self
            .copy_kvm_vcpu_fd(vm)
            .map_err(StartThreadedError::CopyFd)?;
        let interrupt_vcpu_fd = self
            .machine_execution
            .deterministic_interrupts_enabled()
            .then(|| self.copy_kvm_vcpu_fd(vm))
            .transpose()
            .map_err(StartThreadedError::CopyFd)?;
        let execution_locations = self.execution_locations.clone();
        let execution_ledger = self.execution_ledger.clone();
        let machine_execution = self.machine_execution.clone();
        let kick_ready = Arc::new(AtomicBool::new(false));
        let thread_kick_ready = Arc::clone(&kick_ready);
        let vcpu_thread = thread::Builder::new()
            .name(format!("fc_vcpu {}", self.kvm_vcpu.index))
            .spawn(move || {
                let filter = &*seccomp_filter;
                self.register_kick_signal_handler();
                thread_kick_ready.store(true, Ordering::Release);
                // Synchronization to make sure thread local data is initialized.
                barrier.wait();
                self.run(filter);
            })
            .map_err(StartThreadedError::Spawn)?;

        let handle = VcpuHandle::new(
            event_sender,
            response_receiver,
            vcpu_fd,
            execution_locations,
            execution_ledger,
            machine_execution,
            vcpu_thread,
        );
        if let Some(vcpu_fd) = interrupt_vcpu_fd {
            *handle
                .interrupt_kicker
                .lock()
                .expect("vCPU interrupt kicker lock poisoned") = Some(VcpuInterruptKick {
                vcpu_fd,
                pthread: handle.vcpu_thread.as_ref().unwrap().pthread_handle() as usize,
                ready: kick_ready,
            });
            handle
                .machine_execution
                .attach_interrupt_kicker(Arc::downgrade(&handle.interrupt_kicker));
        }
        Ok(handle)
    }

    /// Main loop of the vCPU thread.
    ///
    /// Runs the vCPU in KVM context in a loop. Handles KVM_EXITs then goes back in.
    /// Note that the state of the VCPU and associated VM must be setup first for this to do
    /// anything useful.
    pub fn run(&mut self, seccomp_filter: BpfProgramRef) {
        // Load seccomp filters for this vCPU thread.
        // Execution panics if filters cannot be loaded, use --no-seccomp if skipping filters
        // altogether is the desired behaviour.
        if let Err(err) = crate::seccomp::apply_filter(seccomp_filter) {
            panic!(
                "Failed to set the requested seccomp filters on vCPU {}: Error: {}",
                self.kvm_vcpu.index, err
            );
        }

        // Start running the machine state in the `Paused` state.
        let mut state = VcpuRunState::Paused;
        loop {
            state = match state {
                VcpuRunState::Running => self.running(),
                VcpuRunState::Paused => self.paused(),
                VcpuRunState::Finished => break,
            };
        }
    }

    // This is the main loop of the `Running` state.
    fn running(&mut self) -> VcpuRunState {
        // This loop is here just for optimizing the emulation path.
        // No point in ticking the state machine if there are no external events.
        loop {
            match self.run_emulation() {
                // Emulation ran successfully, continue.
                Ok(VcpuEmulation::Handled) => {
                    // Every handled exit is a guest-visible event we control;
                    // counting them bounds quanta deterministically (Track B′).
                    self.maybe_tick();
                }
                // Emulation was interrupted, check external events.
                Ok(VcpuEmulation::Interrupted) => break,
                // The guest requested a SHUTDOWN or RESET. This is ARM
                // specific. On x86 the i8042 emulation signals the main thread
                // directly without calling Vcpu::exit().
                Ok(VcpuEmulation::Stopped) => return self.exit(FcExitCode::Ok),
                // If the emulation requests a pause lets do this
                #[cfg(feature = "gdb")]
                Ok(VcpuEmulation::Paused) => {
                    #[cfg(target_arch = "x86_64")]
                    self.kvm_vcpu.kvmclock_ctrl();
                    return VcpuRunState::Paused;
                }
                // Emulation errors lead to vCPU exit.
                Err(error) => {
                    error!("vCPU {} emulation failed: {error}", self.kvm_vcpu.index);
                    return self.exit(FcExitCode::GenericError);
                }
            }
        }

        // By default don't change state.
        let mut state = VcpuRunState::Running;

        // Break this emulation loop on any transition request/external event.
        match self.event_receiver.try_recv() {
            // Running ---- Pause ----> Paused
            Ok(VcpuEvent::Pause) => {
                // Capture every explicit Theseus barrier even if the service
                // performed fewer than one sampling quantum of device exits.
                self.record_execution_location();
                self.response_sender
                    .send(VcpuResponse::Paused)
                    .expect("vcpu channel unexpectedly closed");

                #[cfg(target_arch = "x86_64")]
                self.kvm_vcpu.kvmclock_ctrl();

                // Move to 'paused' state.
                state = VcpuRunState::Paused;
            }
            Ok(VcpuEvent::Resume) => {
                self.response_sender
                    .send(VcpuResponse::Resumed)
                    .expect("vcpu channel unexpectedly closed");
            }
            // SaveState cannot be performed on a running Vcpu.
            Ok(VcpuEvent::SaveState) => {
                self.response_sender
                    .send(VcpuResponse::NotAllowed(String::from(
                        "save/restore unavailable while running",
                    )))
                    .expect("vcpu channel unexpectedly closed");
            }
            // DumpCpuConfig cannot be performed on a running Vcpu.
            Ok(VcpuEvent::DumpCpuConfig) => {
                self.response_sender
                    .send(VcpuResponse::NotAllowed(String::from(
                        "cpu config dump is unavailable while running",
                    )))
                    .expect("vcpu channel unexpectedly closed");
            }
            Ok(VcpuEvent::JumpVirtualTime(_)) => {
                self.response_sender
                    .send(VcpuResponse::NotAllowed(String::from(
                        "virtual clock jump is unavailable while running",
                    )))
                    .expect("vcpu channel unexpectedly closed");
            }
            Ok(VcpuEvent::GetVirtualTime) => {
                self.response_sender
                    .send(VcpuResponse::VirtualTime(self.virtual_time_ns()))
                    .expect("vcpu channel unexpectedly closed");
            }
            Ok(VcpuEvent::Finish) => return VcpuRunState::Finished,
            // Unhandled exit of the other end.
            Err(TryRecvError::Disconnected) => {
                // Move to 'exited' state.
                state = self.exit(FcExitCode::GenericError);
            }
            // All other events or lack thereof have no effect on current 'running' state.
            Err(TryRecvError::Empty) => (),
        }

        state
    }

    // This is the main loop of the `Paused` state.
    fn paused(&mut self) -> VcpuRunState {
        match self.event_receiver.recv() {
            // Paused ---- Resume ----> Running
            Ok(VcpuEvent::Resume) => {
                if self.kvm_vcpu.fd.get_kvm_run().immediate_exit == 1u8 {
                    warn!(
                        "Received a VcpuEvent::Resume message with immediate_exit enabled. \
                         immediate_exit was disabled before proceeding"
                    );
                    self.kvm_vcpu.fd.set_kvm_immediate_exit(0);
                }
                self.response_sender
                    .send(VcpuResponse::Resumed)
                    .expect("vcpu channel unexpectedly closed");
                // Move to 'running' state.
                VcpuRunState::Running
            }
            Ok(VcpuEvent::Pause) => {
                // Capture every explicit Theseus barrier even if the service
                // performed fewer than one sampling quantum of device exits.
                self.record_execution_location();
                self.response_sender
                    .send(VcpuResponse::Paused)
                    .expect("vcpu channel unexpectedly closed");
                VcpuRunState::Paused
            }
            Ok(VcpuEvent::SaveState) => {
                // Save vcpu state.
                self.kvm_vcpu
                    .save_state()
                    .map(|vcpu_state| {
                        // Attach Theseus virtual-time bookkeeping (both
                        // arches' VcpuState carry the fields).
                        let vcpu_state = {
                            let mut state = vcpu_state;
                            state.vclock = self.vclock.as_ref().map(|vc| vc.save());
                            state.vclock_exits = self.exits_since_tick;
                            state
                        };
                        self.response_sender
                            .send(VcpuResponse::SavedState(Box::new(vcpu_state)))
                            .expect("vcpu channel unexpectedly closed");
                    })
                    .unwrap_or_else(|err| {
                        self.response_sender
                            .send(VcpuResponse::Error(VcpuError::VcpuResponse(err)))
                            .expect("vcpu channel unexpectedly closed");
                    });

                VcpuRunState::Paused
            }
            Ok(VcpuEvent::DumpCpuConfig) => {
                self.kvm_vcpu
                    .dump_cpu_config()
                    .map(|cpu_config| {
                        self.response_sender
                            .send(VcpuResponse::DumpedCpuConfig(Box::new(cpu_config)))
                            .expect("vcpu channel unexpectedly closed");
                    })
                    .unwrap_or_else(|err| {
                        self.response_sender
                            .send(VcpuResponse::Error(VcpuError::VcpuResponse(err)))
                            .expect("vcpu channel unexpectedly closed");
                    });

                VcpuRunState::Paused
            }
            Ok(VcpuEvent::JumpVirtualTime(delta_ns)) => {
                self.jump_virtual_time(delta_ns)
                    .map(|_| {
                        self.response_sender
                            .send(VcpuResponse::VirtualTimeJumped)
                            .expect("vcpu channel unexpectedly closed");
                    })
                    .unwrap_or_else(|err| {
                        self.response_sender
                            .send(VcpuResponse::Error(err))
                            .expect("vcpu channel unexpectedly closed");
                    });
                VcpuRunState::Paused
            }
            Ok(VcpuEvent::GetVirtualTime) => {
                self.response_sender
                    .send(VcpuResponse::VirtualTime(self.virtual_time_ns()))
                    .expect("vcpu channel unexpectedly closed");
                VcpuRunState::Paused
            }
            Ok(VcpuEvent::Finish) => VcpuRunState::Finished,
            // Unhandled exit of the other end.
            Err(_) => {
                // Move to 'exited' state.
                self.exit(FcExitCode::GenericError)
            }
        }
    }

    // Transition to the exited state and finish on command.
    fn exit(&mut self, exit_code: FcExitCode) -> VcpuRunState {
        self.machine_execution.freeze_admission();
        self.response_sender
            .send(VcpuResponse::Exited(exit_code))
            .expect("vcpu channel unexpectedly closed");
        // Publish the status before waking the event loop. Otherwise it can
        // observe the eventfd but not the response and report a false success.
        if let Err(err) = self.exit_evt.write(1) {
            METRICS.vcpu.failures.inc();
            error!("Failed signaling vcpu exit event: {}", err);
        }
        // From this state we accept a final virtual-clock read or finish.
        loop {
            match self.event_receiver.recv() {
                Ok(VcpuEvent::Finish) => break,
                Ok(VcpuEvent::GetVirtualTime) => self
                    .response_sender
                    .send(VcpuResponse::VirtualTime(self.virtual_time_ns()))
                    .expect("vcpu channel unexpectedly closed"),
                _ => {}
            }
        }
        VcpuRunState::Finished
    }

    /// Runs the vCPU in KVM context and handles the kvm exit reason.
    ///
    /// Returns error or enum specifying whether emulation was handled or interrupted.
    pub fn run_emulation(&mut self) -> Result<VcpuEmulation, VcpuError> {
        if self.machine_execution.shutdown_requested() {
            return Ok(VcpuEmulation::Stopped);
        }
        if self.machine_execution.vcpu_control_requested() {
            return Ok(VcpuEmulation::Interrupted);
        }
        // Theseus: anchor the guest clock before the first KVM_RUN. On
        // aarch64 the counter offset is only writable after vCPU init (which
        // happens at configure time), so this is the earliest safe point.
        if self.vclock.is_some() && !self.vclock_anchored {
            if let Err(err) = self.kvm_vcpu.apply_virtual_time(0) {
                error!("Failed to anchor virtual time: {err:?}");
                METRICS.vcpu.failures.inc();
            }
            self.vclock_anchored = true;
        }

        self.machine_execution
            .deliver_pending_interrupt(
                self.kvm_vcpu.index,
                &self.vm_fd,
                &self.execution_ledger,
            )
            .map_err(VcpuError::FaultyKvmExit)?;

        if self.kvm_vcpu.fd.get_kvm_run().immediate_exit == 1u8 {
            warn!("Requested a vCPU run with immediate_exit enabled. The operation was skipped");
            self.kvm_vcpu.fd.set_kvm_immediate_exit(0);
            return Ok(VcpuEmulation::Interrupted);
        }

        match self.kvm_vcpu.fd.run() {
            Err(ref err) if err.errno() == libc::EINTR => {
                self.kvm_vcpu.fd.set_kvm_immediate_exit(0);
                // Notify that this KVM_RUN was interrupted.
                Ok(VcpuEmulation::Interrupted)
            }
            #[cfg(feature = "gdb")]
            Ok(VcpuExit::Debug(_)) => {
                if let Some(gdb_event) = &self.gdb_event {
                    gdb_event
                        .send(get_raw_tid(self.kvm_vcpu.index.into()))
                        .expect("Unable to notify gdb event");
                }

                Ok(VcpuEmulation::Paused)
            }
            emulation_result => handle_kvm_exit_recorded(
                &mut self.kvm_vcpu.peripherals,
                emulation_result,
                self.kvm_vcpu.index,
                &self.execution_ledger,
                &self.machine_execution,
            ),
        }
    }
}

fn handle_kvm_exit_recorded(
    peripherals: &mut Peripherals,
    emulation_result: Result<VcpuExit<'_>, errno::Error>,
    vcpu: u8,
    ledger: &Arc<Mutex<ExecutionLedger>>,
    machine_execution: &Arc<MachineExecutionController>,
) -> Result<VcpuEmulation, VcpuError> {
    if matches!(&emulation_result, Err(err) if err.errno() == libc::EAGAIN) {
        return handle_kvm_exit(peripherals, emulation_result);
    }
    // This is the first control-plane boundary in the execution stream: only
    // one vCPU may apply guest-visible emulated device effects at a time.
    // During replay, the recorded trace selects which vCPU owns the next turn.
    let mut machine = machine_execution
        .state
        .lock()
        .expect("machine execution controller lock poisoned");
    if machine_execution.shutdown_requested() {
        return Ok(VcpuEmulation::Stopped);
    }
    if machine_execution.vcpu_control_requested() {
        return Ok(VcpuEmulation::Interrupted);
    }
    if let Some(detail) = machine.divergence.clone() {
        return Err(VcpuError::FaultyKvmExit(detail));
    }
    if let Some(expected) = machine.expected.as_ref() {
        let Some(expected_record) = expected.get(machine.position) else {
            // The retained stream is an exact boundary, not merely a prefix.
            // Leave the next KVM exit pending while the control loop observes
            // Pause or Finish.
            return Ok(VcpuEmulation::Interrupted);
        };
        if machine_record_actor(expected_record) != Some(MachineExecutionActor::Vcpu(vcpu)) {
            // Leave the unhandled KVM exit pending and return to the vCPU
            // control loop. Waiting on the replay condition variable here
            // prevents Pause and Finish messages from being observed while a
            // checkpoint boundary is deliberately holding this vCPU behind a
            // recorded host or sibling-vCPU turn.
            return Ok(VcpuEmulation::Interrupted);
        }
    }
    if machine.execution.trace.len() == MACHINE_EXECUTION_TRACE_LIMIT {
        let detail = format!(
            "machine execution trace exceeded {MACHINE_EXECUTION_TRACE_LIMIT} decisions"
        );
        machine.divergence = Some(detail.clone());
        machine_execution.turn_changed.notify_all();
        return Err(VcpuError::FaultyKvmExit(detail));
    }
    // Writes have a complete payload before emulation; read requests have
    // only an address and width. Reject both identities before touching a
    // device. Read values are checked after the device supplies them.
    let admission = match &emulation_result {
        Ok(VcpuExit::MmioWrite(address, data)) => Some((
            format!("vcpu:{vcpu}:mmio_write:{address:#x}:{}:{}", data.len(), hex_bytes(data)),
            true,
        )),
        Ok(VcpuExit::MmioRead(address, data)) => Some((
            format!("vcpu:{vcpu}:mmio_read:{address:#x}:{}:", data.len()),
            false,
        )),
        #[cfg(target_arch = "x86_64")]
        Ok(VcpuExit::IoOut(port, data)) => Some((
            format!("vcpu:{vcpu}:pio_write:{port:#x}:{}:{}", data.len(), hex_bytes(data)),
            true,
        )),
        #[cfg(target_arch = "x86_64")]
        Ok(VcpuExit::IoIn(port, data)) => Some((
            format!("vcpu:{vcpu}:pio_read:{port:#x}:{}:", data.len()),
            false,
        )),
        Ok(exit) => Some((format!("vcpu:{vcpu}:kvm:{exit:?}"), true)),
        Err(_) => None,
    };
    if let (Some(expected), Some((observed, exact))) = (&machine.expected, admission) {
        let matches = expected.get(machine.position).is_some_and(|record| {
            if exact { record == &observed } else { record.starts_with(&observed) }
        });
        if !matches {
            let detail = format!(
                "machine execution replay diverged before device access at decision {}: expected {:?}, observed {observed:?}",
                machine.position, expected.get(machine.position)
            );
            machine.divergence = Some(detail.clone());
            machine_execution.turn_changed.notify_all();
            return Err(VcpuError::FaultyKvmExit(detail));
        }
    }
    let (mut outcome, decision) = match emulation_result {
        Ok(VcpuExit::MmioRead(address, data)) => {
            let outcome = handle_kvm_exit(peripherals, Ok(VcpuExit::MmioRead(address, &mut *data)));
            let decision = format!("mmio_read:{address:#x}:{}:{}", data.len(), hex_bytes(data));
            (outcome, Some(decision))
        }
        #[cfg(target_arch = "x86_64")]
        Ok(VcpuExit::IoIn(port, data)) => {
            let outcome = handle_kvm_exit(peripherals, Ok(VcpuExit::IoIn(port, &mut *data)));
            let decision = format!("pio_read:{port:#x}:{}:{}", data.len(), hex_bytes(data));
            (outcome, Some(decision))
        }
        Ok(VcpuExit::MmioWrite(address, data)) => {
            let decision = format!("mmio_write:{address:#x}:{}:{}", data.len(), hex_bytes(data));
            (
                handle_kvm_exit(peripherals, Ok(VcpuExit::MmioWrite(address, data))),
                Some(decision),
            )
        }
        #[cfg(target_arch = "x86_64")]
        Ok(VcpuExit::IoOut(port, data)) => {
            let decision = format!("pio_write:{port:#x}:{}:{}", data.len(), hex_bytes(data));
            (
                handle_kvm_exit(peripherals, Ok(VcpuExit::IoOut(port, data))),
                Some(decision),
            )
        }
        other => {
            let decision = other.as_ref().ok().map(|exit| format!("kvm:{exit:?}"));
            (handle_kvm_exit(peripherals, other), decision)
        }
    };
    if matches!(&outcome, Ok(VcpuEmulation::Stopped)) {
        machine_execution.request_shutdown();
    } else if machine_execution.shutdown_requested()
        && matches!(&outcome, Ok(VcpuEmulation::Handled))
    {
        outcome = Ok(VcpuEmulation::Stopped);
    }
    if let (Ok(VcpuEmulation::Handled | VcpuEmulation::Stopped), Some(decision)) =
        (&outcome, decision)
    {
        let record = format!("vcpu:{vcpu}:{decision}");
        if let Some(expected) = machine.expected.as_ref() {
            if expected.get(machine.position) != Some(&record) {
                let detail = format!(
                    "machine execution replay diverged at decision {}: expected {:?}, observed {record:?}",
                    machine.position,
                    expected.get(machine.position)
                );
                machine.divergence = Some(detail.clone());
                machine_execution.turn_changed.notify_all();
                return Err(VcpuError::FaultyKvmExit(detail));
            }
        }
        ledger
            .lock()
            .expect("execution ledger lock poisoned")
            .record(decision);
        machine.execution.ledger.record(record.clone());
        machine.execution.trace.push(record);
        machine.position = machine.position.saturating_add(1);
        machine_execution.turn_changed.notify_all();
    }
    outcome
}

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod execution_ledger_tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use kvm_ioctls::VcpuExit;
    use crate::vstate::bus::{Bus, BusDevice};

    use super::{
        EXECUTION_DECISION_TAIL, ExecutionLedger, MachineExecutionController,
        MachineExecutionState,
        Peripherals, VcpuEmulation, handle_kvm_exit_recorded,
    };

    #[derive(Default)]
    struct CountingDevice { reads: usize, writes: usize }

    #[test]
    fn checkpoint_rebuilds_all_hashes_and_preserves_interrupt_order() {
        use super::{CheckpointExecutionState, CheckpointInterrupt};
        let checkpoint = CheckpointExecutionState {
            trace: vec!["vcpu:0:pio_write:0x3f8:1:2a".into(), "host:serial_input:1:41".into(),
                "vcpu:1:mmio_read:0x1000:1:00".into()],
            pending_interrupts: vec![
                CheckpointInterrupt { source: "serial".into(), gsi: 4, coalesce: true },
                CheckpointInterrupt { source: "virtio-mmio".into(), gsi: 5, coalesce: false },
            ],
        };
        let checkpoint: CheckpointExecutionState = serde_json::from_slice(&serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let (machine, local) = checkpoint.restore(2).unwrap();
        let mut expected = ExecutionLedger::default();
        for record in &checkpoint.trace { expected.record(record.clone()); }
        assert_eq!(machine.ledger_evidence(), expected.evidence());
        assert_eq!(local[0].evidence().tail, ["pio_write:0x3f8:1:2a"]);
        assert_eq!(local[1].evidence().tail, ["mmio_read:0x1000:1:00"]);
        assert_eq!(machine.pending_interrupts.iter().map(|irq| (irq.source, irq.gsi)).collect::<Vec<_>>(), [("serial", 4), ("virtio-mmio", 5)]);
        assert!(checkpoint.restore(1).unwrap_err().contains("absent vCPU"));
        let mut invalid = checkpoint;
        invalid.pending_interrupts[0].source = "unknown".into();
        assert!(invalid.restore(2).unwrap_err().contains("unsupported"));
        invalid.pending_interrupts[0].source = "serial".into();
        invalid.pending_interrupts[0].gsi = 1024;
        assert!(invalid.restore(2).is_err());
    }

    #[test]
    fn recorded_interrupt_waits_for_async_completion_before_injection() {
        let controller = Arc::new(MachineExecutionController::default());
        controller.enable_deterministic_interrupts();
        controller.enforce(vec!["vcpu:0:interrupt:virtio-mmio:5".into()]).unwrap();
        let producer = controller.clone();
        let producer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            producer.request_edge_interrupt("virtio-mmio", 5).unwrap();
        });
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        assert!(controller.deliver_pending_interrupt_with(0, &ledger, |gsi| {
            assert_eq!(gsi, 5);
            Ok(())
        }).unwrap());
        producer.join().unwrap();
        assert_eq!(controller.execution_state().trace(), ["vcpu:0:interrupt:virtio-mmio:5"]);
        assert_eq!(ledger.lock().unwrap().evidence().decisions, 1);
        assert!(controller.pending_interrupts_for_test().is_empty());
        assert_eq!(controller.replay_error(), None);
    }

    #[test]
    fn topology_wait_observes_asynchronous_replay_progress() {
        let controller = Arc::new(MachineExecutionController::default());
        controller
            .enforce(vec!["host:serial_input:1:41".into()])
            .unwrap();
        let producer = controller.clone();
        let producer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            producer
                .apply_host_effect("serial_input:1:41".into(), || Ok::<_, ()>(()))
                .unwrap()
                .unwrap();
        });
        assert!(
            controller
                .wait_for_replay_progress(0, std::time::Duration::from_secs(1))
                .unwrap()
        );
        producer.join().unwrap();
        assert_eq!(controller.replay_error(), None);
    }

    #[test]
    fn topology_wait_materializes_a_recorded_interrupt_turn() {
        let controller = MachineExecutionController::default();
        controller.enable_deterministic_interrupts();
        controller
            .enforce(vec!["vcpu:0:interrupt:virtio-mmio:5".into()])
            .unwrap();
        assert!(!controller
            .wait_for_replay_progress(0, std::time::Duration::ZERO)
            .unwrap());
        assert_eq!(
            controller.pending_interrupts_for_test(),
            [("virtio-mmio", 5)]
        );
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        assert!(controller
            .deliver_pending_interrupt_with(0, &ledger, |gsi| {
                assert_eq!(gsi, 5);
                Ok(())
            })
            .unwrap());
        assert_eq!(controller.replay_error(), None);
    }

    #[test]
    fn consecutive_recorded_interrupts_are_injected_before_guest_execution() {
        let controller = MachineExecutionController::default();
        controller.enable_deterministic_interrupts();
        controller
            .enforce(vec![
                "vcpu:0:interrupt:vmgenid:6".into(),
                "vcpu:0:interrupt:vmclock:7".into(),
                "host:serial_input:1:41".into(),
            ])
            .unwrap();
        controller.request_edge_interrupt("vmgenid", 6).unwrap();
        controller.request_edge_interrupt("vmclock", 7).unwrap();
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let mut delivered = Vec::new();
        while controller.may_deliver_another_interrupt(0) {
            assert!(
                controller
                    .deliver_pending_interrupt_with(0, &ledger, |gsi| {
                        delivered.push(gsi);
                        Ok(())
                    })
                    .unwrap()
            );
        }
        assert_eq!(delivered, [6, 7]);
        assert_eq!(
            controller.execution_state().trace(),
            [
                "vcpu:0:interrupt:vmgenid:6",
                "vcpu:0:interrupt:vmclock:7"
            ]
        );
        assert_eq!(controller.replay_error(), Some("machine execution replay stopped at decision 2 of 3".into()));
    }

    #[test]
    fn recording_drains_restored_device_interrupts_before_uart_input() {
        let controller = MachineExecutionController::default();
        controller.enable_deterministic_interrupts();
        controller.request_edge_interrupt("vmclock", 8).unwrap();
        controller.request_interrupt("serial", 4, true).unwrap();
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let mut delivered = Vec::new();

        assert!(
            controller
                .deliver_pending_interrupts_with(0, &ledger, |gsi| {
                    delivered.push(gsi);
                    Ok(())
                })
                .unwrap()
        );

        assert_eq!(delivered, [8, 4]);
        assert_eq!(
            controller.execution_state().trace(),
            [
                "vcpu:0:interrupt:vmclock:8",
                "vcpu:0:interrupt:serial:4"
            ]
        );
        assert!(controller.pending_interrupts_for_test().is_empty());
    }

    #[test]
    fn replay_selects_the_recorded_interrupt_from_pending_requests() {
        let controller = Arc::new(MachineExecutionController::default());
        controller.enable_deterministic_interrupts();
        controller
            .enforce(vec!["vcpu:0:interrupt:virtio-mmio:5".into()])
            .unwrap();
        controller.request_edge_interrupt("serial", 4).unwrap();
        let producer = controller.clone();
        let producer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            producer.request_edge_interrupt("virtio-mmio", 5).unwrap();
        });
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        assert!(
            controller
                .deliver_pending_interrupt_with(0, &ledger, |gsi| {
                    assert_eq!(gsi, 5);
                    Ok(())
                })
                .unwrap()
        );
        producer.join().unwrap();
        assert_eq!(
            controller.execution_state().trace(),
            ["vcpu:0:interrupt:virtio-mmio:5"]
        );
        assert_eq!(controller.pending_interrupts_for_test(), [("serial", 4)]);
        assert_eq!(ledger.lock().unwrap().evidence().decisions, 1);
        assert_eq!(controller.replay_error(), None);
    }

    #[test]
    fn pending_interrupt_waits_behind_an_earlier_same_vcpu_device_effect() {
        let controller = MachineExecutionController::default();
        controller.enable_deterministic_interrupts();
        controller
            .enforce(vec![
                "vcpu:0:pio_write:0x3f9:1:05".into(),
                "vcpu:0:interrupt:serial:4".into(),
            ])
            .unwrap();
        controller.request_edge_interrupt("serial", 4).unwrap();
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        assert!(
            !controller
                .deliver_pending_interrupt_with(0, &ledger, |_| {
                    panic!("interrupt injected before the recorded device access")
                })
                .unwrap()
        );
        assert_eq!(controller.execution_state().trace().len(), 0);
        assert_eq!(controller.pending_interrupts_for_test(), [("serial", 4)]);
        assert_eq!(controller.replay_divergence(), None);
    }

    #[test]
    fn terminal_request_wakes_a_vcpu_waiting_for_recorded_interrupt() {
        let controller = Arc::new(MachineExecutionController::default());
        controller.enable_deterministic_interrupts();
        controller.enforce(vec!["vcpu:0:interrupt:virtio-mmio:5".into()]).unwrap();
        let stop = controller.clone();
        let stop = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            stop.request_shutdown();
        });
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        assert!(!controller.deliver_pending_interrupt_with(0, &ledger, |_| {
            panic!("post-terminal interrupt injected")
        }).unwrap());
        stop.join().unwrap();
        assert_eq!(controller.execution_state().trace().len(), 0);
        assert_eq!(ledger.lock().unwrap().evidence().decisions, 0);
    }

    #[test]
    fn control_request_wakes_a_vcpu_waiting_for_recorded_interrupt() {
        let controller = Arc::new(MachineExecutionController::default());
        controller.enable_deterministic_interrupts();
        controller
            .enforce(vec!["vcpu:0:interrupt:virtio-mmio:5".into()])
            .unwrap();
        let control = controller.clone();
        let control = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            control.request_vcpu_control();
        });
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        assert!(
            !controller
                .deliver_pending_interrupt_with(0, &ledger, |_| {
                    panic!("interrupt injected while vCPU control was pending")
                })
                .unwrap()
        );
        control.join().unwrap();
        assert!(controller.vcpu_control_requested());
        controller.resume_vcpu_execution();
        assert!(!controller.vcpu_control_requested());
        assert_eq!(controller.replay_divergence(), None);
    }

    impl BusDevice for CountingDevice {
        fn read(&mut self, _: u64, _: u64, data: &mut [u8]) {
            self.reads += 1;
            data.fill(42);
        }
        fn write(&mut self, _: u64, _: u64, _: &[u8]) -> Option<Arc<std::sync::Barrier>> {
            self.writes += 1;
            None
        }
    }

    fn counting_device() -> (Peripherals, Arc<Mutex<CountingDevice>>) {
        let device = Arc::new(Mutex::new(CountingDevice::default()));
        let bus = Arc::new(Bus::new());
        bus.insert(device.clone(), 0x10, 0x10).unwrap();
        let mut peripherals = Peripherals::default();
        peripherals.mmio_bus = Some(bus);
        (peripherals, device)
    }

    #[test]
    fn changed_write_is_rejected_before_any_device_effect() {
        let (mut peripherals, device) = counting_device();
        let controller = Arc::new(MachineExecutionController::default());
        controller.enforce(vec!["vcpu:0:mmio_write:0x10:1:2a".into()]).unwrap();
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let error = handle_kvm_exit_recorded(&mut peripherals,
            Ok(VcpuExit::MmioWrite(0x10, &[43])), 0, &ledger, &controller).unwrap_err();
        assert!(error.to_string().contains("before device access"));
        assert_eq!(device.lock().unwrap().writes, 0);
        assert_eq!(ledger.lock().unwrap().evidence().decisions, 0);
        assert!(controller.execution_state().trace().is_empty());
        assert!(controller.replay_error().unwrap().contains("decision 0"));
        assert_eq!(controller.replay_divergence(), controller.replay_error());
    }

    #[test]
    fn changed_read_identity_does_not_consume_device_input() {
        for (address, width) in [(0x11, 1), (0x10, 2)] {
            let (mut peripherals, device) = counting_device();
            let controller = Arc::new(MachineExecutionController::default());
            controller.enforce(vec!["vcpu:0:mmio_read:0x10:1:2a".into()]).unwrap();
            let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
            let mut data = vec![0xff; width];
            assert!(handle_kvm_exit_recorded(&mut peripherals,
                Ok(VcpuExit::MmioRead(address, &mut data)), 0, &ledger, &controller).is_err());
            assert_eq!(device.lock().unwrap().reads, 0);
            assert_eq!(data, vec![0xff; width]);
        }
    }

    #[test]
    fn changed_read_value_keeps_the_first_divergence_and_no_admitted_record() {
        let (mut peripherals, device) = counting_device();
        let controller = Arc::new(MachineExecutionController::default());
        controller.enforce(vec!["vcpu:0:mmio_read:0x10:1:00".into()]).unwrap();
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let mut data = [0xff];
        let error = handle_kvm_exit_recorded(&mut peripherals,
            Ok(VcpuExit::MmioRead(0x10, &mut data)), 0, &ledger, &controller).unwrap_err();
        assert_eq!(device.lock().unwrap().reads, 1);
        assert_eq!(data, [42]);
        assert_eq!(ledger.lock().unwrap().evidence().decisions, 0);
        let first = controller.replay_error().unwrap();
        assert!(first.contains("observed"));
        assert!(error.to_string().contains(&first));
        assert!(handle_kvm_exit_recorded(&mut peripherals,
            Ok(VcpuExit::MmioWrite(0x10, &[0])), 0, &ledger, &controller).is_err());
        assert_eq!(controller.replay_error().as_deref(), Some(first.as_str()));
        assert_eq!(device.lock().unwrap().writes, 0);
    }

    #[test]
    fn full_trace_rejects_the_next_effect_before_emulation() {
        let (mut peripherals, device) = counting_device();
        let controller = Arc::new(MachineExecutionController::default());
        controller.state.lock().unwrap().execution.trace = vec![String::new(); super::MACHINE_EXECUTION_TRACE_LIMIT];
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        assert!(handle_kvm_exit_recorded(&mut peripherals,
            Ok(VcpuExit::MmioWrite(0x10, &[42])), 0, &ledger, &controller).is_err());
        assert_eq!(device.lock().unwrap().writes, 0);
        assert_eq!(ledger.lock().unwrap().evidence().decisions, 0);
    }

    #[test]
    fn system_shutdown_is_one_terminal_turn_and_rejects_later_host_input() {
        let controller = Arc::new(MachineExecutionController::default());
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let mut peripherals = Peripherals::default();
        let exit = VcpuExit::SystemEvent(kvm_bindings::KVM_SYSTEM_EVENT_SHUTDOWN, &[]);
        let expected = format!("vcpu:0:kvm:{exit:?}");
        controller.enforce(vec![expected.clone()]).unwrap();
        assert_eq!(handle_kvm_exit_recorded(&mut peripherals, Ok(exit), 0, &ledger, &controller).unwrap(), VcpuEmulation::Stopped);
        assert!(controller.shutdown_requested());
        assert_eq!(controller.execution_state().trace(), [expected]);
        assert!(controller.apply_host_effect("control_event:90".into(), || -> Result<(), ()> { panic!("post-exit input admitted") }).is_err());
        assert_eq!(controller.replay_error(), None);
    }

    #[cfg(target_arch = "x86_64")]
    fn reset_device(controller: &Arc<MachineExecutionController>) -> (
        Peripherals,
        vmm_sys_util::eventfd::EventFd,
        Arc<Mutex<crate::devices::legacy::I8042Device>>,
    ) {
        let event = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let device = crate::devices::legacy::I8042Device::new(event.try_clone().unwrap()).unwrap();
        device.attach_reset_controller(controller);
        // Bus routes hold weak links; retain the device as a real manager does.
        let device = Arc::new(Mutex::new(device));
        let bus = Arc::new(Bus::new());
        bus.insert(device.clone(), 0x60, 5).unwrap();
        let mut peripherals = Peripherals::default();
        peripherals.pio_bus = Some(bus);
        (peripherals, event, device)
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn i8042_reset_ends_the_trace_synchronously_without_shutdown_polling() {
        let controller = Arc::new(MachineExecutionController::default());
        let (mut peripherals, event, _device) = reset_device(&controller);
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        controller.enforce(vec!["vcpu:0:pio_write:0x64:1:fe".into()]).unwrap();
        assert_eq!(handle_kvm_exit_recorded(&mut peripherals,
            Ok(VcpuExit::IoOut(0x64, &[0xfe])), 0, &ledger, &controller).unwrap(), VcpuEmulation::Stopped);
        assert_eq!(event.read().unwrap_err().raw_os_error(), Some(libc::EAGAIN));
        let terminal = controller.execution_state().ledger_evidence();
        for id in [0, 1] {
            let mut data = [0xff];
            assert_eq!(handle_kvm_exit_recorded(&mut peripherals,
                Ok(VcpuExit::IoIn(0x64, &mut data)), id, &ledger, &controller).unwrap(), VcpuEmulation::Stopped);
            assert_eq!(data, [0xff]);
        }
        assert_eq!(terminal, controller.execution_state().ledger_evidence());
        assert_eq!(ledger.lock().unwrap().evidence().decisions, 1);
        assert_eq!(controller.replay_error(), None);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn mismatched_reset_never_stops_the_vm_or_signals_the_eventloop() {
        let controller = Arc::new(MachineExecutionController::default());
        let (mut peripherals, event, _device) = reset_device(&controller);
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        controller.enforce(vec!["vcpu:0:pio_write:0x64:1:20".into()]).unwrap();
        assert!(handle_kvm_exit_recorded(&mut peripherals,
            Ok(VcpuExit::IoOut(0x64, &[0xfe])), 0, &ledger, &controller).is_err());
        assert!(!controller.shutdown_requested());
        assert_eq!(event.read().unwrap_err().raw_os_error(), Some(libc::EAGAIN));
        assert_eq!(controller.execution_state().trace().len(), 0);
    }

    #[test]
    fn hashes_ordered_length_framed_decisions() {
        let mut first = ExecutionLedger::default();
        first.record("mmio_read:0x10:1".to_owned());
        first.record("mmio_write:0x20:1:41".to_owned());

        let mut same = ExecutionLedger::default();
        same.record("mmio_read:0x10:1".to_owned());
        same.record("mmio_write:0x20:1:41".to_owned());
        assert_eq!(first.evidence(), same.evidence());

        let mut reordered = ExecutionLedger::default();
        reordered.record("mmio_write:0x20:1:41".to_owned());
        reordered.record("mmio_read:0x10:1".to_owned());
        assert_ne!(first.evidence().sha256, reordered.evidence().sha256);
    }

    #[test]
    fn cloned_branch_continues_the_parent_digest_and_bounds_the_readable_tail() {
        let mut parent = ExecutionLedger::default();
        for index in 0..EXECUTION_DECISION_TAIL + 4 {
            parent.record(format!("decision:{index}"));
        }
        let mut child = parent.clone();
        child.record("child".to_owned());

        let evidence = child.evidence();
        assert_eq!(evidence.decisions, (EXECUTION_DECISION_TAIL + 5) as u64);
        assert_eq!(evidence.tail.len(), EXECUTION_DECISION_TAIL);
        assert_eq!(evidence.tail.last().map(String::as_str), Some("child"));
        assert_ne!(evidence.sha256, parent.evidence().sha256);
    }

    #[test]
    fn active_replay_accepts_an_inherited_prefix_and_requires_the_suffix() {
        let mut ledger = ExecutionLedger::default();
        ledger.record("vcpu:0:mmio_write:0x10:1:2a".to_owned());
        let controller = MachineExecutionController::default();
        controller.restore_execution_state(MachineExecutionState {
            ledger,
            trace: vec!["vcpu:0:mmio_write:0x10:1:2a".to_owned()],
            pending_interrupts: VecDeque::new(),
        });
        controller
            .enforce(vec![
                "vcpu:0:mmio_write:0x10:1:2a".to_owned(),
                "vcpu:1:mmio_read:0x20:1:00".to_owned(),
            ])
            .unwrap();
        assert_eq!(
            controller.replay_error().as_deref(),
            Some("machine execution replay stopped at decision 1 of 2")
        );
        assert_eq!(controller.replay_divergence(), None);
    }

    #[test]
    fn pending_interrupts_are_coalesced_and_survive_branch_state_restore() {
        let source = MachineExecutionController::default();
        source.enable_deterministic_interrupts();
        source.request_interrupt("serial", 4, true).unwrap();
        source.request_interrupt("serial", 4, true).unwrap();
        source.request_edge_interrupt("vmclock", 6).unwrap();
        source.request_edge_interrupt("vmclock", 6).unwrap();

        let state = source.execution_state();
        assert_eq!(state.pending_interrupts.len(), 3);
        assert_eq!(state.pending_interrupts[0].source, "serial");
        assert_eq!(state.pending_interrupts[0].gsi, 4);
        assert_eq!(state.pending_interrupts[1].source, "vmclock");
        assert_eq!(state.pending_interrupts[2].source, "vmclock");

        let restored = MachineExecutionController::default();
        restored.request_edge_interrupt("vmgenid", 5).unwrap();
        restored.restore_execution_state(state);
        let restored_state = restored.execution_state();
        assert_eq!(restored_state.pending_interrupts.len(), 4);
        assert_eq!(restored_state.pending_interrupts[0].source, "serial");
        assert_eq!(restored_state.pending_interrupts[0].gsi, 4);
        assert_eq!(restored_state.pending_interrupts[3].source, "vmgenid");
        assert_eq!(restored_state.pending_interrupts[3].gsi, 5);
    }

    #[test]
    fn interrupt_kicker_links_do_not_keep_vcpu_handles_alive() {
        let controller = MachineExecutionController::default();
        let kicker = Arc::new(Mutex::new(None));
        controller.attach_interrupt_kicker(Arc::downgrade(&kicker));
        controller.request_edge_interrupt("vmclock", 6).unwrap();
        assert_eq!(Arc::strong_count(&kicker), 1);

        drop(kicker);
        controller.request_edge_interrupt("vmclock", 6).unwrap();
        assert!(controller.interrupt_kickers.lock().unwrap().is_empty());
    }

    #[test]
    fn active_replay_rejects_a_different_checkpoint_prefix() {
        let controller = MachineExecutionController::default();
        controller.restore_execution_state(MachineExecutionState {
            ledger: ExecutionLedger::default(),
            trace: vec!["vcpu:0:mmio_write:0x10:1:2a".to_owned()],
            pending_interrupts: VecDeque::new(),
        });
        assert!(
            controller
                .enforce(vec!["vcpu:1:mmio_write:0x10:1:2a".to_owned()])
                .unwrap_err()
                .contains("before replay began")
        );
    }

    #[test]
    fn active_replay_admits_exact_host_effects() {
        let controller = MachineExecutionController::default();
        controller
            .enforce(vec!["host:serial_input:2:2a0a".to_owned()])
            .unwrap();
        controller
            .apply_host_effect("serial_input:2:2a0a".to_owned(), || Ok::<(), ()>(()))
            .unwrap()
            .unwrap();

        assert_eq!(
            controller.execution_state().trace(),
            ["host:serial_input:2:2a0a"]
        );
        assert_eq!(controller.replay_error(), None);
    }

    #[test]
    fn execution_ledger_host_turn_yields_to_vcpu_control_messages() {
        let mut peripherals = Peripherals::default();
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let controller = Arc::new(MachineExecutionController::default());
        controller
            .enforce(vec![
                "host:serial_input:1:41".to_owned(),
                "vcpu:0:mmio_write:0x10:1:2a".to_owned(),
            ])
            .unwrap();

        let result = handle_kvm_exit_recorded(
            &mut peripherals,
            Ok(VcpuExit::MmioWrite(0x10, &[0x2a])),
            0,
            &ledger,
            &controller,
        )
        .unwrap();
        assert_eq!(result, VcpuEmulation::Interrupted);
        assert!(controller.execution_state().trace().is_empty());
        assert_eq!(controller.replay_divergence(), None);

        controller
            .apply_host_effect("serial_input:1:41".to_owned(), || Ok::<(), ()>(()))
            .unwrap()
            .unwrap();
        let result = handle_kvm_exit_recorded(
            &mut peripherals,
            Ok(VcpuExit::MmioWrite(0x10, &[0x2a])),
            0,
            &ledger,
            &controller,
        )
        .unwrap();
        assert_eq!(result, VcpuEmulation::Handled);
        assert_eq!(
            controller.execution_state().trace(),
            ["host:serial_input:1:41", "vcpu:0:mmio_write:0x10:1:2a"]
        );
        assert_eq!(controller.replay_error(), None);
    }

    #[test]
    fn completed_replay_yields_without_extending_the_retained_stream() {
        let (mut peripherals, device) = counting_device();
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let controller = Arc::new(MachineExecutionController::default());
        controller
            .enforce(vec!["host:serial_input:1:41".to_owned()])
            .unwrap();
        controller
            .apply_host_effect("serial_input:1:41".to_owned(), || Ok::<(), ()>(()))
            .unwrap()
            .unwrap();

        let result = handle_kvm_exit_recorded(
            &mut peripherals,
            Ok(VcpuExit::MmioWrite(0x10, &[0x2a])),
            0,
            &ledger,
            &controller,
        )
        .unwrap();
        assert_eq!(result, VcpuEmulation::Interrupted);
        assert_eq!(device.lock().unwrap().writes, 0);
        assert_eq!(
            controller.execution_state().trace(),
            ["host:serial_input:1:41"]
        );
        assert_eq!(controller.replay_error(), None);
    }

    #[test]
    fn active_replay_rejects_a_changed_host_effect_before_delivery() {
        let controller = MachineExecutionController::default();
        controller
            .enforce(vec!["host:virtual_time_jump:1000".to_owned()])
            .unwrap();

        let mut delivered = false;
        let error = controller
            .apply_host_effect("virtual_time_jump:2000".to_owned(), || {
                delivered = true;
                Ok::<(), ()>(())
            })
            .unwrap_err();
        assert!(error.contains("expected"));
        assert!(error.contains("virtual_time_jump:1000"));
        assert!(!delivered);
        assert!(controller.replay_error().is_some());
    }

    #[test]
    fn failed_host_delivery_does_not_advance_the_stream() {
        let controller = MachineExecutionController::default();
        let delivery = controller
            .apply_host_effect("control_event:90".to_owned(), || Err::<(), _>("full"))
            .unwrap();

        assert_eq!(delivery, Err("full"));
        assert!(controller.execution_state().trace().is_empty());
    }

    #[test]
    fn active_replay_rejects_malformed_host_effects() {
        let controller = MachineExecutionController::default();
        assert!(controller.enforce(vec!["host:".to_owned()]).is_err());
        assert!(
            MachineExecutionController::default()
                .enforce(vec!["host:ctrl_alt_del".to_owned()])
                .is_ok()
        );
        assert!(controller
            .enforce(vec!["host:serial_input:2:2a".to_owned()])
            .is_err());
        assert!(controller
            .enforce(vec!["host:unknown:payload".to_owned()])
            .is_err());
        assert!(controller
            .enforce(vec!["host:serial_input:0:".to_owned()])
            .is_err());
        assert!(controller
            .enforce(vec!["host:control_event:AF".to_owned()])
            .is_err());
        assert!(controller.enforce(vec!["vcpu:0:".to_owned()]).is_err());
        assert!(controller
            .enforce(vec!["vcpu:00:mmio_read:0x0:1:00".to_owned()])
            .is_err());
        assert!(controller
            .enforce(vec!["vcpu:0:interrupt:unknown:4".to_owned()])
            .is_err());
        assert!(controller
            .enforce(vec!["vcpu:0:interrupt:serial:04".to_owned()])
            .is_err());
    }
}

/// Handle the return value of a call to [`VcpuFd::run`] and update our emulation accordingly
fn handle_kvm_exit(
    peripherals: &mut Peripherals,
    emulation_result: Result<VcpuExit, errno::Error>,
) -> Result<VcpuEmulation, VcpuError> {
    match emulation_result {
        Ok(run) => match run {
            VcpuExit::MmioRead(addr, data) => {
                data.fill(0);
                if let Some(mmio_bus) = &peripherals.mmio_bus {
                    let _metric = METRICS.vcpu.exit_mmio_read_agg.record_latency_metrics();
                    if let Err(err) = mmio_bus.read(addr, data) {
                        warn!("Invalid MMIO read @ {addr:#x}:{:#x}: {err}", data.len());
                    }
                    METRICS.vcpu.exit_mmio_read.inc();
                }
                Ok(VcpuEmulation::Handled)
            }
            VcpuExit::MmioWrite(addr, data) => {
                if let Some(mmio_bus) = &peripherals.mmio_bus {
                    let _metric = METRICS.vcpu.exit_mmio_write_agg.record_latency_metrics();
                    if let Err(err) = mmio_bus.write(addr, data) {
                        warn!("Invalid MMIO read @ {addr:#x}:{:#x}: {err}", data.len());
                    }
                    METRICS.vcpu.exit_mmio_write.inc();
                }
                Ok(VcpuEmulation::Handled)
            }
            // Documentation specifies that below kvm exits are considered
            // errors.
            VcpuExit::FailEntry(hardware_entry_failure_reason, cpu) => {
                // Hardware entry failure.
                METRICS.vcpu.failures.inc();
                error!(
                    "Received KVM_EXIT_FAIL_ENTRY signal: {} on cpu {}",
                    hardware_entry_failure_reason, cpu
                );
                Err(VcpuError::FaultyKvmExit(format!(
                    "{:?}",
                    VcpuExit::FailEntry(hardware_entry_failure_reason, cpu)
                )))
            }
            VcpuExit::InternalError => {
                // Failure from the Linux KVM subsystem rather than from the hardware.
                METRICS.vcpu.failures.inc();
                error!("Received KVM_EXIT_INTERNAL_ERROR signal");
                Err(VcpuError::FaultyKvmExit(format!(
                    "{:?}",
                    VcpuExit::InternalError
                )))
            }
            VcpuExit::SystemEvent(event_type, event_flags) => match event_type {
                KVM_SYSTEM_EVENT_RESET | KVM_SYSTEM_EVENT_SHUTDOWN => {
                    info!(
                        "Received KVM_SYSTEM_EVENT: type: {}, event: {:?}",
                        event_type, event_flags
                    );
                    Ok(VcpuEmulation::Stopped)
                }
                _ => {
                    METRICS.vcpu.failures.inc();
                    error!(
                        "Received KVM_SYSTEM_EVENT signal type: {}, flag: {:?}",
                        event_type, event_flags
                    );
                    Err(VcpuError::FaultyKvmExit(format!(
                        "{:?}",
                        VcpuExit::SystemEvent(event_type, event_flags)
                    )))
                }
            },
            arch_specific_reason => {
                // run specific architecture emulation.
                peripherals.run_arch_emulation(arch_specific_reason)
            }
        },
        // The unwrap on raw_os_error can only fail if we have a logic
        // error in our code in which case it is better to panic.
        Err(ref err) => match err.errno() {
            libc::EAGAIN => Ok(VcpuEmulation::Handled),
            libc::ENOSYS => {
                METRICS.vcpu.failures.inc();
                error!("Received ENOSYS error because KVM failed to emulate an instruction.");
                Err(VcpuError::FaultyKvmExit(
                    "Received ENOSYS error because KVM failed to emulate an instruction."
                        .to_string(),
                ))
            }
            _ => {
                METRICS.vcpu.failures.inc();
                error!("Failure during vcpu run: {}", err);
                Err(VcpuError::FaultyKvmExit(format!("{}", err)))
            }
        },
    }
}

/// List of events that the Vcpu can receive.
#[derive(Debug, Clone)]
pub enum VcpuEvent {
    /// The vCPU thread will end when receiving this message.
    Finish,
    /// Pause the Vcpu.
    Pause,
    /// Event to resume the Vcpu.
    Resume,
    /// Event to save the state of a paused Vcpu.
    SaveState,
    /// Event to dump CPU configuration of a paused Vcpu.
    DumpCpuConfig,
    /// Advance a paused vCPU's deterministic virtual clock.
    JumpVirtualTime(u64),
    /// Read a vCPU's deterministic virtual clock at its next event boundary.
    GetVirtualTime,
}

/// List of responses that the Vcpu reports.
pub enum VcpuResponse {
    /// Requested action encountered an error.
    Error(VcpuError),
    /// Vcpu is stopped.
    Exited(FcExitCode),
    /// Requested action not allowed.
    NotAllowed(String),
    /// Vcpu is paused.
    Paused,
    /// Vcpu is resumed.
    Resumed,
    /// Vcpu state is saved.
    SavedState(Box<VcpuState>),
    /// Vcpu is in the state where CPU config is dumped.
    DumpedCpuConfig(Box<CpuConfiguration>),
    /// A virtual-clock jump was applied while paused.
    VirtualTimeJumped,
    /// Current deterministic virtual-clock time in nanoseconds, when enabled.
    VirtualTime(Option<u64>),
}

impl fmt::Debug for VcpuResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use crate::VcpuResponse::*;
        match self {
            Paused => write!(f, "VcpuResponse::Paused"),
            Resumed => write!(f, "VcpuResponse::Resumed"),
            Exited(code) => write!(f, "VcpuResponse::Exited({:?})", code),
            SavedState(_) => write!(f, "VcpuResponse::SavedState"),
            Error(err) => write!(f, "VcpuResponse::Error({:?})", err),
            NotAllowed(reason) => write!(f, "VcpuResponse::NotAllowed({})", reason),
            DumpedCpuConfig(_) => write!(f, "VcpuResponse::DumpedCpuConfig"),
            VirtualTimeJumped => write!(f, "VcpuResponse::VirtualTimeJumped"),
            VirtualTime(value) => write!(f, "VcpuResponse::VirtualTime({value:?})"),
        }
    }
}

/// Wrapper over Vcpu that hides the underlying interactions with the Vcpu thread.
#[derive(Debug)]
pub struct VcpuHandle {
    event_sender: Sender<VcpuEvent>,
    response_receiver: Receiver<VcpuResponse>,
    /// VcpuFd
    pub vcpu_fd: VcpuFd,
    execution_locations: Arc<Mutex<BTreeSet<u64>>>,
    execution_ledger: Arc<Mutex<ExecutionLedger>>,
    machine_execution: Arc<MachineExecutionController>,
    interrupt_kicker: Arc<Mutex<Option<VcpuInterruptKick>>>,
    // Rust JoinHandles have to be wrapped in Option if you ever plan on 'join()'ing them.
    // We want to be able to join these threads in tests.
    vcpu_thread: Option<thread::JoinHandle<()>>,
}

/// Error type for [`VcpuHandle::send_event`].
#[derive(Debug, derive_more::From, thiserror::Error)]
#[error("Failed to signal vCPU: {0}")]
pub struct VcpuSendEventError(pub vmm_sys_util::errno::Error);

impl VcpuHandle {
    /// Creates a new [`VcpuHandle`].
    ///
    /// # Arguments
    /// + `event_sender`: [`Sender`] to communicate [`VcpuEvent`] to control the vcpu.
    /// + `response_received`: [`Received`] from which the vcpu's responses can be read.
    /// + `vcpu_thread`: A [`JoinHandle`] for the vcpu thread.
    pub fn new(
        event_sender: Sender<VcpuEvent>,
        response_receiver: Receiver<VcpuResponse>,
        vcpu_fd: VcpuFd,
        execution_locations: Arc<Mutex<BTreeSet<u64>>>,
        execution_ledger: Arc<Mutex<ExecutionLedger>>,
        machine_execution: Arc<MachineExecutionController>,
        vcpu_thread: thread::JoinHandle<()>,
    ) -> Self {
        Self {
            event_sender,
            response_receiver,
            vcpu_fd,
            execution_locations,
            execution_ledger,
            machine_execution,
            interrupt_kicker: Arc::new(Mutex::new(None)),
            vcpu_thread: Some(vcpu_thread),
        }
    }
    /// Sends event to vCPU.
    ///
    /// # Errors
    ///
    /// When [`vmm_sys_util::linux::signal::Killable::kill`] errors.
    pub fn send_event(&mut self, event: VcpuEvent) -> Result<(), VcpuSendEventError> {
        match &event {
            VcpuEvent::Pause | VcpuEvent::Finish => {
                self.machine_execution.request_vcpu_control();
            }
            VcpuEvent::Resume => self.machine_execution.resume_vcpu_execution(),
            _ => {}
        }
        // Use expect() to crash if the other thread closed this channel.
        self.event_sender
            .send(event)
            .expect("event sender channel closed on vcpu end.");
        self.kick()
    }

    /// Interrupt `KVM_RUN` so this vCPU can observe newly queued machine work.
    pub(crate) fn kick(&mut self) -> Result<(), VcpuSendEventError> {
        // Kick the vcpu so it picks up the message.
        // Add a fence to ensure the write is visible to the vpu thread
        self.vcpu_fd.set_kvm_immediate_exit(1);
        fence(Ordering::Release);
        self.vcpu_thread
            .as_ref()
            // Safe to unwrap since constructor make this 'Some'.
            .unwrap()
            .kill(sigrtmin() + VCPU_RTSIG_OFFSET)?;
        Ok(())
    }

    /// Returns a reference to the [`Received`] from which the vcpu's responses can be read.
    pub fn response_receiver(&self) -> &Receiver<VcpuResponse> {
        &self.response_receiver
    }

    /// Stable location samples gathered while this vCPU ran normally.
    pub fn execution_locations(&self) -> Vec<u64> {
        self.execution_locations
            .lock()
            .expect("execution coverage lock poisoned")
            .iter()
            .copied()
            .collect()
    }

    /// Seed a restored child with its parent branch's coverage history.
    pub fn extend_execution_locations(&self, locations: impl IntoIterator<Item = u64>) {
        self.execution_locations
            .lock()
            .expect("execution coverage lock poisoned")
            .extend(locations);
    }

    /// Clone the complete rolling execution ledger for a campaign branch.
    pub fn execution_ledger(&self) -> ExecutionLedger {
        self.execution_ledger
            .lock()
            .expect("execution ledger lock poisoned")
            .clone()
    }

    /// Continue a restored campaign branch from its parent's exact ledger.
    pub fn seed_execution_ledger(&self, ledger: ExecutionLedger) {
        *self
            .execution_ledger
            .lock()
            .expect("execution ledger lock poisoned") = ledger;
    }

    /// Clone the shared VM-wide execution ledger.
    pub fn machine_execution_ledger(&self) -> ExecutionLedger {
        self.machine_execution.execution_state().ledger
    }

    /// Clone the complete bounded machine state for a campaign branch.
    pub fn machine_execution_state(&self) -> MachineExecutionState {
        self.machine_execution.execution_state()
    }

    /// Return every retained machine decision needed for active replay.
    pub fn machine_execution_trace(&self) -> Vec<String> {
        self.machine_execution.execution_state().trace
    }

    /// Continue a restored VM from its parent's machine-wide state.
    pub fn seed_machine_execution_state(&self, state: MachineExecutionState) {
        self.machine_execution.restore_execution_state(state);
    }

    /// Gate subsequent exits against an exact retained machine trace.
    pub fn enforce_machine_execution_trace(&self, trace: Vec<String>) -> Result<(), String> {
        self.machine_execution.enforce(trace)
    }

    /// Return a recorded mismatch or incomplete expected suffix.
    pub fn machine_execution_replay_error(&self) -> Option<String> {
        self.machine_execution.replay_error()
    }

    /// Return an actual mismatch without treating an in-progress suffix as an error.
    pub fn machine_execution_replay_divergence(&self) -> Option<String> {
        self.machine_execution.replay_divergence()
    }

    /// Briefly yield a replaying vCPU to asynchronous device completion.
    pub fn wait_for_machine_execution_replay_progress(
        &self,
        position: usize,
        timeout: Duration,
    ) -> Result<bool, String> {
        self.machine_execution
            .wait_for_replay_progress(position, timeout)
    }

    /// Clone the controller used to serialize host and vCPU effects.
    pub(crate) fn machine_execution_controller(&self) -> Arc<MachineExecutionController> {
        Arc::clone(&self.machine_execution)
    }
}

// Wait for the Vcpu thread to finish execution
impl Drop for VcpuHandle {
    fn drop(&mut self) {
        self.interrupt_kicker
            .lock()
            .expect("vCPU interrupt kicker lock poisoned")
            .take();
        // The vCPU thread owns the response sender, so the channel disconnects
        // once it exits. Wait for that disconnect (draining any stale responses)
        // with a timeout rather than joining unconditionally, so a thread that
        // never finished (e.g. a missed Finish event) fails fast instead of
        // hanging teardown forever.
        let thread = self.vcpu_thread.take().unwrap();
        loop {
            match self.response_receiver.recv_timeout(VCPU_JOIN_TIMEOUT) {
                // Sender dropped: the thread has exited.
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let name = thread.thread().name().unwrap_or("<unnamed>");
                    panic!("Timed out waiting for vCPU thread '{name}' to exit")
                }
                // Unexpected: a response was still queued at teardown. Discard
                // it and keep waiting for the thread to exit.
                Ok(response) => {
                    warn!("Discarding unexpected vCPU response during teardown: {response:?}");
                }
            }
        }
        thread.join().unwrap();
    }
}

/// Vcpu emulation state.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum VcpuEmulation {
    /// Handled.
    Handled,
    /// Interrupted.
    Interrupted,
    /// Stopped.
    Stopped,
    /// Pause request
    #[cfg(feature = "gdb")]
    Paused,
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    #[cfg(target_arch = "x86_64")]
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Barrier, Mutex};

    use linux_loader::loader::KernelLoader;
    use vmm_sys_util::errno;

    use super::*;
    use crate::RECV_TIMEOUT_SEC;
    use crate::arch::{BootProtocol, EntryPoint};
    use crate::seccomp::get_empty_filters;
    use crate::utils::mib_to_bytes;
    use crate::utils::signal::validate_signal_num;
    use crate::vstate::bus::BusDevice;
    use crate::vstate::memory::{GuestAddress, GuestMemoryMmap};
    use crate::vstate::vcpu::VcpuError as EmulationError;
    use crate::vstate::vm::tests::setup_vm_with_memory;

    struct DummyDevice;

    impl BusDevice for DummyDevice {
        fn read(&mut self, _base: u64, _offset: u64, _data: &mut [u8]) {}

        fn write(&mut self, _base: u64, _offset: u64, _data: &[u8]) -> Option<Arc<Barrier>> {
            None
        }
    }

    #[test]
    fn test_handle_kvm_exit() {
        let (_, mut vcpu) = setup_vcpu(0x1000);
        let res = handle_kvm_exit(&mut vcpu.kvm_vcpu.peripherals, Ok(VcpuExit::Hlt));
        assert!(matches!(
            res,
            Err(EmulationError::UnhandledKvmExit(s)) if s == "Hlt",
        ));

        let res = handle_kvm_exit(&mut vcpu.kvm_vcpu.peripherals, Ok(VcpuExit::Shutdown));
        assert!(matches!(
            res,
            Err(EmulationError::UnhandledKvmExit(s)) if s == "Shutdown",
        ));

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Ok(VcpuExit::FailEntry(0, 0)),
        );
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit("FailEntry(0, 0)".to_string())
            )
        );

        let res = handle_kvm_exit(&mut vcpu.kvm_vcpu.peripherals, Ok(VcpuExit::InternalError));
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit("InternalError".to_string())
            )
        );

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Ok(VcpuExit::SystemEvent(2, &[])),
        );
        assert_eq!(res.unwrap(), VcpuEmulation::Stopped);

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Ok(VcpuExit::SystemEvent(1, &[])),
        );
        assert_eq!(res.unwrap(), VcpuEmulation::Stopped);

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Ok(VcpuExit::SystemEvent(3, &[])),
        );
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit("SystemEvent(3, [])".to_string())
            )
        );

        // Check what happens with an unhandled exit reason.
        let res = handle_kvm_exit(&mut vcpu.kvm_vcpu.peripherals, Ok(VcpuExit::Unknown));
        assert_eq!(
            res.unwrap_err().to_string(),
            "Unexpected kvm exit received: Unknown".to_string()
        );

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Err(errno::Error::new(libc::EAGAIN)),
        );
        assert_eq!(res.unwrap(), VcpuEmulation::Handled);

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Err(errno::Error::new(libc::ENOSYS)),
        );
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit(
                    "Received ENOSYS error because KVM failed to emulate an instruction."
                        .to_string()
                )
            )
        );

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Err(errno::Error::new(libc::EINVAL)),
        );
        assert_eq!(
            format!("{:?}", res.unwrap_err()),
            format!(
                "{:?}",
                EmulationError::FaultyKvmExit("Invalid argument (os error 22)".to_string())
            )
        );

        let bus = Arc::new(Bus::new());
        let dummy = Arc::new(Mutex::new(DummyDevice));
        bus.insert(dummy, 0x10, 0x10).unwrap();
        vcpu.set_mmio_bus(bus);
        let addr = 0x10;

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Ok(VcpuExit::MmioRead(addr, &mut [0, 0, 0, 0])),
        );
        assert_eq!(res.unwrap(), VcpuEmulation::Handled);

        let res = handle_kvm_exit(
            &mut vcpu.kvm_vcpu.peripherals,
            Ok(VcpuExit::MmioWrite(addr, &[0, 0, 0, 0])),
        );
        assert_eq!(res.unwrap(), VcpuEmulation::Handled);
    }

    #[test]
    fn recorded_exit_includes_emulated_read_bytes_and_ignores_eagain() {
        let (_, mut vcpu) = setup_vcpu(0x1000);
        let ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let machine_execution = Arc::new(MachineExecutionController::default());
        let mut data = [0xff, 0xff];
        let result = handle_kvm_exit_recorded(
            &mut vcpu.kvm_vcpu.peripherals,
            Ok(VcpuExit::MmioRead(0x10, &mut data)),
            1,
            &ledger,
            &machine_execution,
        );
        assert_eq!(result.unwrap(), VcpuEmulation::Handled);
        assert_eq!(data, [0, 0]);
        let evidence = ledger.lock().unwrap().evidence();
        assert_eq!(evidence.decisions, 1);
        assert_eq!(evidence.tail, ["mmio_read:0x10:2:0000"]);
        assert_eq!(
            machine_execution.execution_state().ledger_evidence().tail,
            ["vcpu:1:mmio_read:0x10:2:0000"]
        );

        let second_vcpu_ledger = Arc::new(Mutex::new(ExecutionLedger::default()));
        let result = handle_kvm_exit_recorded(
            &mut vcpu.kvm_vcpu.peripherals,
            Ok(VcpuExit::MmioWrite(0x10, &[0x2a])),
            0,
            &second_vcpu_ledger,
            &machine_execution,
        );
        assert_eq!(result.unwrap(), VcpuEmulation::Handled);
        assert_eq!(
            machine_execution.execution_state().ledger_evidence().tail,
            [
                "vcpu:1:mmio_read:0x10:2:0000",
                "vcpu:0:mmio_write:0x10:1:2a"
            ]
        );

        let result = handle_kvm_exit_recorded(
            &mut vcpu.kvm_vcpu.peripherals,
            Err(errno::Error::new(libc::EAGAIN)),
            1,
            &ledger,
            &machine_execution,
        );
        assert_eq!(result.unwrap(), VcpuEmulation::Handled);
        assert_eq!(ledger.lock().unwrap().evidence().decisions, 1);
        assert_eq!(
            machine_execution
                .execution_state()
                .ledger_evidence()
                .decisions,
            2
        );
    }

    impl PartialEq for VcpuResponse {
        fn eq(&self, other: &Self) -> bool {
            use crate::VcpuResponse::*;
            // Guard match with no wildcard to make sure we catch new enum variants.
            match self {
                Paused | Resumed | Exited(_) => (),
                Error(_) | NotAllowed(_) | SavedState(_) | DumpedCpuConfig(_)
                | VirtualTimeJumped | VirtualTime(_) => (),
            };
            match (self, other) {
                (Paused, Paused) | (Resumed, Resumed) => true,
                (Exited(code), Exited(other_code)) => code == other_code,
                (NotAllowed(_), NotAllowed(_))
                | (SavedState(_), SavedState(_))
                | (DumpedCpuConfig(_), DumpedCpuConfig(_))
                | (VirtualTimeJumped, VirtualTimeJumped) => true,
                (VirtualTime(value), VirtualTime(other_value)) => value == other_value,
                (Error(err), Error(other_err)) => {
                    format!("{:?}", err) == format!("{:?}", other_err)
                }
                _ => false,
            }
        }
    }

    // Auxiliary function being used throughout the tests.
    #[allow(unused_mut)]
    pub(crate) fn setup_vcpu(mem_size: usize) -> (KvmVm, Vcpu) {
        let mut vm = setup_vm_with_memory(mem_size);

        let mut vcpus = vm.create_vcpus(1).unwrap();
        let mut vcpu = vcpus.remove(0);

        #[cfg(target_arch = "aarch64")]
        vcpu.kvm_vcpu.init(&[]).unwrap();

        (vm, vcpu)
    }

    fn load_good_kernel(vm_memory: &GuestMemoryMmap) -> GuestAddress {
        use std::fs::File;
        use std::path::PathBuf;

        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        #[cfg(target_arch = "x86_64")]
        path.push("src/test_utils/mock_resources/test_elf.bin");
        #[cfg(target_arch = "aarch64")]
        path.push("src/test_utils/mock_resources/test_pe.bin");

        let mut kernel_file = File::open(path).expect("Cannot open kernel file");

        #[cfg(target_arch = "x86_64")]
        let entry_addr = linux_loader::loader::elf::Elf::load(
            vm_memory,
            Some(GuestAddress(crate::arch::get_kernel_start())),
            &mut kernel_file,
            Some(GuestAddress(crate::arch::get_kernel_start())),
        )
        .unwrap();
        #[cfg(target_arch = "aarch64")]
        let entry_addr =
            linux_loader::loader::pe::PE::load(vm_memory, None, &mut kernel_file, None).unwrap();
        entry_addr.kernel_load
    }

    fn vcpu_configured_for_boot() -> (KvmVm, VcpuHandle, EventFd) {
        // Need enough mem to boot linux.
        let mem_size = mib_to_bytes(64);
        let (vm, mut vcpu) = setup_vcpu(mem_size);

        let vcpu_exit_evt = vcpu.exit_evt.try_clone().unwrap();

        // Needs a kernel since we'll actually run this vcpu.
        let entry_point = EntryPoint {
            entry_addr: load_good_kernel(vm.guest_memory()),
            protocol: BootProtocol::LinuxBoot,
            // `setup_header` is only a field of `EntryPoint` on x86_64.
            #[cfg(target_arch = "x86_64")]
            setup_header: None,
        };

        #[cfg(target_arch = "x86_64")]
        {
            use crate::cpu_config::x86_64::cpuid::Cpuid;
            let cpuid = Cpuid::try_from(vm.kvm().supported_cpuid.clone()).unwrap();
            let configured_cpuid = vcpu
                .kvm_vcpu
                .configure_cpuid(&cpuid, 1, false)
                .expect("failed to configure vcpu CPUID");
            vcpu.kvm_vcpu
                .configure_msrs_for_boot(&BTreeMap::new(), &configured_cpuid)
                .expect("failed to configure vcpu MSRs");
            vcpu.kvm_vcpu
                .configure_boot_state(vm.guest_memory(), entry_point)
                .expect("failed to configure vcpu");
        }

        #[cfg(target_arch = "aarch64")]
        vcpu.kvm_vcpu
            .configure(
                vm.guest_memory(),
                entry_point,
                &VcpuConfig {
                    vcpu_count: 1,
                    smt: false,
                    cpu_config: crate::cpu_config::aarch64::CpuConfiguration::default(),
                },
                &vm.kvm().optional_capabilities(),
            )
            .expect("failed to configure vcpu");

        let mut seccomp_filters = get_empty_filters();
        let barrier = Arc::new(Barrier::new(2));
        let vcpu_handle = vcpu
            .start_threaded(
                &vm,
                seccomp_filters.remove("vcpu").unwrap(),
                barrier.clone(),
            )
            .expect("failed to start vcpu");
        // Wait for vCPUs to initialize their TLS before moving forward.
        barrier.wait();

        (vm, vcpu_handle, vcpu_exit_evt)
    }

    #[test]
    fn test_set_mmio_bus() {
        let (_, mut vcpu) = setup_vcpu(0x1000);
        assert!(vcpu.kvm_vcpu.peripherals.mmio_bus.is_none());
        vcpu.set_mmio_bus(Arc::new(Bus::new()));
        assert!(vcpu.kvm_vcpu.peripherals.mmio_bus.is_some());
    }

    #[test]
    fn test_vcpu_kick() {
        let (vm, mut vcpu) = setup_vcpu(0x1000);

        let mut kvm_run =
            kvm_ioctls::KvmRunWrapper::mmap_from_fd(&vcpu.kvm_vcpu.fd, vm.fd().run_size())
                .expect("cannot mmap kvm-run");
        let vcpu_kvm_run =
            kvm_ioctls::KvmRunWrapper::mmap_from_fd(&vcpu.kvm_vcpu.fd, vm.fd().run_size())
                .expect("cannot mmap kvm-run");
        let success = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let vcpu_success = success.clone();
        let barrier = Arc::new(Barrier::new(2));
        let vcpu_barrier = barrier.clone();
        // Start Vcpu thread which will be kicked with a signal.
        let handle = std::thread::Builder::new()
            .name("test_vcpu_kick".to_string())
            .spawn(move || {
                vcpu.register_kick_signal_handler();
                // Notify TLS was populated.
                vcpu_barrier.wait();
                // Loop for max 1 second to check if the signal handler has run.
                for _ in 0..10 {
                    if vcpu_kvm_run.as_ref().immediate_exit == 1 {
                        // Signal handler has run and set immediate_exit to 1.
                        vcpu_success.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .expect("cannot start thread");
        barrier.wait();

        // Set immediate_exit and kick the Vcpu using the custom signal.
        kvm_run.as_mut_ref().immediate_exit = 1;
        handle
            .kill(sigrtmin() + VCPU_RTSIG_OFFSET)
            .expect("failed to signal thread");
        handle.join().expect("failed to join thread");
        // Verify that the Vcpu saw its kvm immediate-exit as set.
        assert!(success.load(Ordering::Acquire));
    }

    // Sends an event to a vcpu and expects a particular response.
    fn queue_event_expect_response(
        handle: &mut VcpuHandle,
        event: VcpuEvent,
        response: VcpuResponse,
    ) {
        handle
            .send_event(event)
            .expect("failed to send event to vcpu");
        assert_eq!(
            handle
                .response_receiver()
                .recv_timeout(RECV_TIMEOUT_SEC)
                .expect("did not receive event response from vcpu"),
            response
        );
    }

    #[test]
    fn test_immediate_exit_shortcircuits_execution() {
        let (_, mut vcpu) = setup_vcpu(0x1000);

        vcpu.kvm_vcpu.fd.set_kvm_immediate_exit(1);
        // Set a dummy value to be returned by the emulate call
        let result = vcpu.run_emulation().expect("Failed to run emulation");
        assert_eq!(
            result,
            VcpuEmulation::Interrupted,
            "The Immediate Exit short-circuit should have prevented the execution of emulate"
        );

        let event_sender = vcpu.event_sender.take().expect("vCPU already started");
        let _ = event_sender.send(VcpuEvent::Resume);
        vcpu.kvm_vcpu.fd.set_kvm_immediate_exit(1);
        // paused is expected to coerce immediate_exit to 0 when receiving a VcpuEvent::Resume
        let _ = vcpu.paused();
        assert_eq!(
            0,
            vcpu.kvm_vcpu.fd.get_kvm_run().immediate_exit,
            "Immediate Exit should have been disabled by sending Resume to a paused VM"
        )
    }

    #[test]
    fn test_vcpu_pause_resume() {
        let (_vm, mut vcpu_handle, vcpu_exit_evt) = vcpu_configured_for_boot();

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue a Pause event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Validate vcpu handled the EINTR gracefully and didn't exit.
        let err = vcpu_exit_evt.read().unwrap_err();
        assert_eq!(err.raw_os_error().unwrap(), libc::EAGAIN);

        // Queue another Pause event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue another Resume event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue another Pause event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        vcpu_handle.send_event(VcpuEvent::Finish).unwrap();
    }

    #[test]
    fn test_vcpu_save_state_events() {
        let (_vm, mut vcpu_handle, _vcpu_exit_evt) = vcpu_configured_for_boot();

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue a SaveState event, expect a response.
        queue_event_expect_response(
            &mut vcpu_handle,
            VcpuEvent::SaveState,
            VcpuResponse::NotAllowed(String::new()),
        );

        // Queue another Pause event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Pause, VcpuResponse::Paused);

        // Queue a SaveState event, get the response.
        vcpu_handle
            .send_event(VcpuEvent::SaveState)
            .expect("failed to send event to vcpu");
        match vcpu_handle
            .response_receiver()
            .recv_timeout(RECV_TIMEOUT_SEC)
            .expect("did not receive event response from vcpu")
        {
            VcpuResponse::SavedState(_) => {}
            _ => panic!("unexpected response"),
        };

        vcpu_handle.send_event(VcpuEvent::Finish).unwrap();
    }

    #[test]
    fn test_vcpu_dump_cpu_config() {
        let (_vm, mut vcpu_handle, _) = vcpu_configured_for_boot();

        // Queue a DumpCpuConfig event, expect a DumpedCpuConfig response.
        vcpu_handle
            .send_event(VcpuEvent::DumpCpuConfig)
            .expect("Failed to send an event to vcpu.");
        match vcpu_handle
            .response_receiver()
            .recv_timeout(RECV_TIMEOUT_SEC)
            .expect("Could not receive a response from vcpu.")
        {
            VcpuResponse::DumpedCpuConfig(_) => (),
            VcpuResponse::Error(err) => panic!("Got an error: {err}"),
            _ => panic!("Got an unexpected response."),
        }

        // Queue a Resume event, expect a response.
        queue_event_expect_response(&mut vcpu_handle, VcpuEvent::Resume, VcpuResponse::Resumed);

        // Queue a DumpCpuConfig event, expect a NotAllowed respoonse.
        // The DumpCpuConfig event is only allowed while paused.
        queue_event_expect_response(
            &mut vcpu_handle,
            VcpuEvent::DumpCpuConfig,
            VcpuResponse::NotAllowed(String::new()),
        );

        vcpu_handle.send_event(VcpuEvent::Finish).unwrap();
    }

    #[test]
    fn test_vcpu_rtsig_offset() {
        validate_signal_num(sigrtmin() + VCPU_RTSIG_OFFSET).unwrap();
    }
}
