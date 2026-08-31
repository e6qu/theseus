// Copyright 2026 Theseus contributors.
// SPDX-License-Identifier: Apache-2.0

//! Single-step code coverage: the set of guest PCs executed, collected with
//! `KVM_GUESTDBG_SINGLESTEP` — true code coverage with zero guest
//! instrumentation. Slow by nature (one VM exit per instruction), so this is
//! the ground-truth signal for small workloads and the validation reference
//! for faster mechanisms later.
//!
//! The deterministic property this buys the explorer: the same timeline
//! replayed produces the same coverage set, and a timeline that diverges
//! produces a different one.

use std::collections::BTreeSet;

use kvm_ioctls::VcpuFd;

/// Errors from coverage collection.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum CoverageError {
    /// KVM error enabling/disabling guest debug: {0}
    GuestDebug(kvm_ioctls::Error),
    /// KVM error running the vCPU: {0}
    Run(kvm_ioctls::Error),
    /// KVM error reading the PC: {0}
    ReadPc(kvm_ioctls::Error),
    /// KVM error writing the PC: {0}
    WritePc(kvm_ioctls::Error),
    /// MMIO exits cannot be skipped on x86_64 (variable-length instructions);
    /// the collector currently supports aarch64 guests only.
    UnsupportedMmioSkip,
}

/// Collected coverage: unique guest PCs and total instructions stepped.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// Unique guest program counters observed.
    pub pcs: BTreeSet<u64>,
    /// Total instructions single-stepped.
    pub steps: usize,
}

/// Read the guest PC (aarch64: CNTVCT-style one-reg; x86_64: rip).
#[cfg(target_arch = "aarch64")]
fn read_pc(vcpu: &VcpuFd) -> Result<u64, CoverageError> {
    let mut buf = [0u8; 8];
    vcpu.get_one_reg(crate::arch::aarch64::regs::PC, &mut buf)
        .map_err(CoverageError::ReadPc)?;
    Ok(u64::from_ne_bytes(buf))
}

/// Read the guest PC (x86_64: rip from KVM_GET_REGS).
#[cfg(target_arch = "x86_64")]
fn read_pc(vcpu: &VcpuFd) -> Result<u64, CoverageError> {
    Ok(vcpu.get_regs().map_err(CoverageError::ReadPc)?.rip)
}

/// Skip the current instruction (aarch64: fixed 4-byte width). Used for MMIO
/// exits while single-stepping without devices attached: the guest's memory
/// instruction is counted as covered and bypassed (reads yield whatever the
/// destination register held — coverage, not functional emulation).
#[cfg(target_arch = "aarch64")]
fn skip_instruction(vcpu: &VcpuFd, pc: u64) -> Result<(), CoverageError> {
    vcpu.set_one_reg(crate::arch::aarch64::regs::PC, &(pc + 4).to_ne_bytes())
        .map_err(CoverageError::WritePc)?;
    Ok(())
}

/// Collect executed guest PCs by single-stepping the vCPU.
///
/// Stops after `max_steps` instructions, or when the last `loop_window` steps
/// produced no new PC (the guest is parked in a tight loop).
///
/// The caller owns setup: guest code loaded, PC set, vCPU initialized. The
/// vCPU must be stopped/paused; debug state is restored (disabled) on return.
pub fn collect(vcpu: &mut VcpuFd, max_steps: usize, loop_window: usize) -> Result<Coverage, CoverageError> {
    use kvm_bindings::{KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, kvm_guest_debug};

    let debug_on = kvm_guest_debug {
        control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP,
        ..Default::default()
    };
    vcpu.set_guest_debug(&debug_on).map_err(CoverageError::GuestDebug)?;

    let mut coverage = Coverage::default();
    let mut steps_without_new_pc = 0usize;

    let result = loop {
        if coverage.steps >= max_steps || steps_without_new_pc >= loop_window {
            break Ok(());
        }
        match vcpu.run() {
            Ok(kvm_ioctls::VcpuExit::Debug(_)) => {
                let pc = read_pc(vcpu)?;
                coverage.steps += 1;
                if coverage.pcs.insert(pc) {
                    steps_without_new_pc = 0;
                } else {
                    steps_without_new_pc += 1;
                }
            }
            // HLT or shutdown: the guest is done.
            Ok(kvm_ioctls::VcpuExit::Hlt) | Ok(kvm_ioctls::VcpuExit::Shutdown) => break Ok(()),
            // MMIO exit while stepping without devices: count the instruction
            // as covered and skip it (else the vCPU re-exits forever).
            Ok(kvm_ioctls::VcpuExit::MmioRead(_, _))
            | Ok(kvm_ioctls::VcpuExit::MmioWrite(_, _)) => {
                #[cfg(target_arch = "aarch64")]
                {
                    let pc = read_pc(vcpu)?;
                    coverage.steps += 1;
                    if coverage.pcs.insert(pc) {
                        steps_without_new_pc = 0;
                    } else {
                        steps_without_new_pc += 1;
                    }
                    skip_instruction(vcpu, pc)?;
                }
                #[cfg(target_arch = "x86_64")]
                break Err(CoverageError::UnsupportedMmioSkip);
            }
            Ok(_) => { /* other exits: not a step; keep going */ }
            Err(err) => break Err(CoverageError::Run(err)),
        }
    };

    let debug_off = kvm_guest_debug {
        control: 0,
        ..Default::default()
    };
    vcpu.set_guest_debug(&debug_off)
        .map_err(CoverageError::GuestDebug)?;

    result.map(|_| coverage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-step the bare-metal Theseus guest and collect its PCs.
    /// Requires KVM.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_single_step_coverage_is_deterministic() {
        use crate::arch::aarch64::regs::PC;
        use crate::test_utils::single_region_mem_at_raw;
        use crate::vstate::vm::tests::setup_vm;
        use vm_memory::Bytes;

        let guest = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/test_utils/mock_resources/theseus_guest.bin"
        ))
        .unwrap();

        let collect_once = || {
            let mut vm = setup_vm();
            let mem = single_region_mem_at_raw(crate::arch::DRAM_MEM_START, 0x100000);
            vm.register_dram_memory_regions(mem).unwrap();
            let mut vcpu = crate::arch::aarch64::vcpu::KvmVcpu::new(0, &vm).unwrap();
            vcpu.init(&[]).unwrap();
            vm.setup_irqchip(1).unwrap();

            vm.guest_memory()
                .write_slice(&guest, crate::vstate::memory::GuestAddress(crate::arch::DRAM_MEM_START))
                .unwrap();
            vcpu.fd
                .set_one_reg(PC, &crate::arch::DRAM_MEM_START.to_ne_bytes())
                .unwrap();

            collect(&mut vcpu.fd, 10_000, 32).unwrap()
        };

        let a = collect_once();
        let b = collect_once();

        // Deterministic: identical coverage across runs.
        assert_eq!(a, b, "single-step coverage must be replay-identical");

        // Sane: the guest is ~60 instructions to its event loop; the entry
        // branch target (DRAM start + 64-byte header) must be covered.
        assert!(
            a.pcs.contains(&(crate::arch::DRAM_MEM_START + 0x40)),
            "entry point not in coverage: {:?}",
            a.pcs
        );
        assert!(a.pcs.len() > 20, "suspiciously little coverage: {:?}", a.pcs);
        assert!(a.steps > 20, "guest ran {} steps", a.steps);
    }

    /// A guest with a redirected entry branch (skip the print loops, jump
    /// straight to the marker section) must produce different coverage than
    /// the original. Requires KVM.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_coverage_detects_divergence() {
        use crate::arch::aarch64::regs::PC;
        use crate::test_utils::single_region_mem_at_raw;
        use crate::vstate::vm::tests::setup_vm;
        use vm_memory::Bytes;

        let original = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/test_utils/mock_resources/theseus_guest.bin"
        ))
        .unwrap();
        // Redirect the entry branch (offset 0): `b 0x40` -> `b 0xc0`
        // (aarch64 `b` = imm26 << 2; 0x14000010 -> 0x14000030).
        let mut redirected = original.clone();
        redirected[0..4].copy_from_slice(&0x14000030u32.to_le_bytes());

        let collect_with = |code: &[u8]| {
            let mut vm = setup_vm();
            let mem = single_region_mem_at_raw(crate::arch::DRAM_MEM_START, 0x100000);
            vm.register_dram_memory_regions(mem).unwrap();
            let mut vcpu = crate::arch::aarch64::vcpu::KvmVcpu::new(0, &vm).unwrap();
            vcpu.init(&[]).unwrap();
            vm.setup_irqchip(1).unwrap();
            vm.guest_memory()
                .write_slice(code, crate::vstate::memory::GuestAddress(crate::arch::DRAM_MEM_START))
                .unwrap();
            vcpu.fd
                .set_one_reg(PC, &crate::arch::DRAM_MEM_START.to_ne_bytes())
                .unwrap();
            collect(&mut vcpu.fd, 10_000, 32).unwrap()
        };

        let a = collect_with(&original);
        let b = collect_with(&redirected);

        // The banner print loop lives right after the header branch target;
        // the redirected guest never executes it.
        let banner_loop_pc = crate::arch::DRAM_MEM_START + 0x4c;
        assert!(a.pcs.contains(&banner_loop_pc), "original should print");
        assert!(
            !b.pcs.contains(&banner_loop_pc),
            "redirected guest must skip the banner loop"
        );
        // Both reach the marker section.
        let marker_pc = crate::arch::DRAM_MEM_START + 0xc0;
        assert!(a.pcs.contains(&marker_pc) && b.pcs.contains(&marker_pc));
        assert_ne!(a.pcs, b.pcs, "divergent guests must diverge in coverage");
    }
}
