// The userspace APIC pair for a no-irqchip VM: the local APIC over its
// x2APIC MSRs and the IOAPIC over its MMIO page. Without
// `KVM_CREATE_IRQCHIP`, KVM runs the vCPU with pure userspace interrupt
// injection - every timer and GSI-routed delivery becomes a recorded
// stream turn asserted through `KVM_INTERRUPT` (see
// docs/design/deterministic-timer-arming.md, option B).

use std::sync::{Arc, Mutex};

use crate::vstate::bus::BusDevice;

/// `IA32_APIC_BASE`: the guest's local APIC base and mode bits.
pub const MSR_IA32_APIC_BASE: u32 = 0x1B;
/// The TSC-deadline MSR: the arming register Linux prefers.
pub const MSR_IA32_TSC_DEADLINE: u32 = 0x6E0;
/// First x2APIC MSR (register offset 0x00).
pub const MSR_X2APIC_FIRST: u32 = 0x800;
/// Last x2APIC MSR (register offset 0x3F0).
pub const MSR_X2APIC_LAST: u32 = 0x83F;

/// The x2APIC MSR indices this local APIC answers (0x800 + offset/16).
pub const MSR_X2APIC_TPR: u32 = 0x808;
pub const MSR_X2APIC_EOI: u32 = 0x80B;
pub const MSR_X2APIC_SPIV: u32 = 0x80F;
pub const MSR_X2APIC_ICR: u32 = 0x830;
pub const MSR_X2APIC_LVT_TIMER: u32 = 0x832;
pub const MSR_X2APIC_TMICT: u32 = 0x838;
pub const MSR_X2APIC_TMCCT: u32 = 0x839;
pub const MSR_X2APIC_TDCR: u32 = 0x83E;
pub const MSR_X2APIC_SELF_IPI: u32 = 0x83F;

/// LVT timer entry: the vector mask bit.
const LVT_MASKED: u64 = 1 << 16;
/// LVT timer entry: the mode field (bits 18:17): one-shot, periodic,
/// TSC-deadline.
const LVT_MODE_SHIFT: u64 = 17;
const MODE_TSC_DEADLINE: u64 = 2;
const MODE_PERIODIC: u64 = 1;

/// The IOAPIC's MMIO base: the address the ACPI MADT declares.
pub const IOAPIC_MMIO_BASE: u64 = 0xFEC0_0000;
/// The IOAPIC's MMIO page size (the page the ACPI MADT declares).
pub const IOAPIC_MMIO_SIZE: u64 = 0x1000;
/// The IOAPIC register select is at MMIO offset 0x00, the data window at
/// offset 0x10.
const IOREGSEL_OFFSET: u64 = 0x00;
const IOWIN_OFFSET: u64 = 0x10;
/// The IOAPIC's redirection table starts at register 0x10, two 32-bit
/// registers per entry.
const IOAPIC_FIRST_REDIRECTION: u8 = 0x10;
/// The version register value: version 0x11, 24 redirection entries.
const IOAPIC_VERSION: u32 = 0x0017_0011;

/// Errors from the userspace APIC pair.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum UserspaceApicError {
    /// MSR {0:#x} is outside the emulated local-APIC range
    UnknownMsr(u32),
}

/// The guest's local APIC, emulated from its x2APIC MSRs.
///
/// The timer lives entirely in the virtual clock: TSC-deadline and
/// TMICT-mode deadlines are absolute virtual TSC ticks, and
/// [`UserspaceLapic::timer_due`] asserts the delivery at the first
/// handled exit at or after the deadline - no host hrtimer anywhere.
#[derive(Debug, Default)]
pub struct UserspaceLapic {
    apic_base: u64,
    spurious: u64,
    tpr: u64,
    lvt_timer: u64,
    initial_count: u64,
    divide_config: u64,
    icr: u64,
    /// The absolute virtual TSC tick the TSC-deadline mode fires at.
    tsc_deadline: Option<u64>,
    /// The absolute virtual TSC tick the TMICT (one-shot or periodic)
    /// mode fires at.
    tmict_expiry: Option<u64>,
    /// The TMICT-mode period in virtual TSC ticks (initial count times
    /// the divider), kept for periodic re-arming.
    tmict_period: Option<u64>,
    eoi_count: u64,
}

impl UserspaceLapic {
    /// Whether `index` is an MSR this local APIC owns.
    pub fn owns_msr(index: u32) -> bool {
        index == MSR_IA32_APIC_BASE
            || index == MSR_IA32_TSC_DEADLINE
            || (MSR_X2APIC_FIRST..=MSR_X2APIC_LAST).contains(&index)
    }

    /// The divide-value encoding of the TDCR register: how many TSC ticks
    /// one timer-count tick spans.
    fn divider(tdcr: u64) -> u64 {
        match tdcr & 0xF {
            0xB => 1,
            0x0 => 2,
            0x1 => 4,
            0x2 => 8,
            0x3 => 16,
            0x8 => 32,
            0x9 => 64,
            0xA => 128,
            _ => 2,
        }
    }

    fn timer_vector(&self) -> Option<u8> {
        if self.lvt_timer & LVT_MASKED != 0 {
            return None;
        }
        let vector = (self.lvt_timer & 0xFF) as u8;
        (vector != 0).then_some(vector)
    }

    /// Reads an MSR. `tsc_now` is the guest's current virtual TSC tick.
    pub fn read_msr(&mut self, index: u32, tsc_now: u64) -> Result<u64, UserspaceApicError> {
        let value = match index {
            MSR_IA32_APIC_BASE => self.apic_base,
            MSR_IA32_TSC_DEADLINE => self.tsc_deadline.unwrap_or(0),
            MSR_X2APIC_TPR => self.tpr,
            MSR_X2APIC_EOI => 0,
            MSR_X2APIC_SPIV => self.spurious,
            MSR_X2APIC_ICR => self.icr,
            MSR_X2APIC_LVT_TIMER => self.lvt_timer,
            MSR_X2APIC_TMICT => self.initial_count,
            MSR_X2APIC_TMCCT => {
                let mode = (self.lvt_timer >> LVT_MODE_SHIFT) & 0x3;
                if mode == MODE_TSC_DEADLINE {
                    self.tsc_deadline.unwrap_or(0).saturating_sub(tsc_now)
                } else {
                    self.tmict_expiry
                        .unwrap_or(0)
                        .saturating_sub(tsc_now)
                        / Self::divider(self.divide_config)
                }
            }
            MSR_X2APIC_TDCR => self.divide_config,
            MSR_X2APIC_SELF_IPI => 0,
            MSR_X2APIC_FIRST..=MSR_X2APIC_LAST => 0,
            _ => return Err(UserspaceApicError::UnknownMsr(index)),
        };
        Ok(value)
    }

    /// Writes an MSR. `tsc_now` is the guest's current virtual TSC tick.
    pub fn write_msr(
        &mut self,
        index: u32,
        value: u64,
        tsc_now: u64,
    ) -> Result<(), UserspaceApicError> {
        match index {
            MSR_IA32_APIC_BASE => self.apic_base = value,
            MSR_IA32_TSC_DEADLINE => {
                // Zero is the disarm the clockevent layer writes between
                // periods; it never schedules a delivery.
                self.tsc_deadline = (value != 0).then_some(value);
            }
            MSR_X2APIC_TPR => self.tpr = value & 0xFF,
            MSR_X2APIC_EOI => self.eoi_count += 1,
            MSR_X2APIC_SPIV => self.spurious = value,
            MSR_X2APIC_ICR => self.icr = value,
            MSR_X2APIC_LVT_TIMER => self.lvt_timer = value,
            MSR_X2APIC_TMICT => {
                self.initial_count = value;
                self.tmict_expiry = if value == 0 {
                    self.tmict_period = None;
                    None
                } else {
                    let period = value * Self::divider(self.divide_config);
                    self.tmict_period = Some(period);
                    Some(tsc_now + period)
                };
            }
            MSR_X2APIC_TDCR => self.divide_config = value & 0xF,
            MSR_X2APIC_SELF_IPI => self.icr = value,
            MSR_X2APIC_FIRST..=MSR_X2APIC_LAST => {}
            _ => return Err(UserspaceApicError::UnknownMsr(index)),
        }
        Ok(())
    }

    /// The timer assertion at a handled-exit boundary: when the deadline
    /// has passed, disarms (or re-arms, periodic mode) and returns the
    /// vector to inject.
    pub fn timer_due(&mut self, tsc_now: u64) -> Option<u8> {
        let vector = self.timer_vector()?;
        let mode = (self.lvt_timer >> LVT_MODE_SHIFT) & 0x3;
        if mode == MODE_TSC_DEADLINE {
            let due = self.tsc_deadline.is_some_and(|deadline| tsc_now >= deadline);
            if due {
                self.tsc_deadline = None;
                return Some(vector);
            }
            return None;
        }
        let expiry = self.tmict_expiry?;
        if tsc_now < expiry {
            return None;
        }
        if mode == MODE_PERIODIC
            && let Some(period) = self.tmict_period
        {
            // Re-arm at the next multiple that is in the future, so a
            // run of late boundaries advances whole periods - the guest
            // observes the ticks it asked for, at virtual positions.
            let missed = (tsc_now - expiry) / period + 1;
            self.tmict_expiry = Some(expiry + missed * period);
            return Some(vector);
        }
        self.tmict_expiry = None;
        Some(vector)
    }
}

/// The APIC's MMIO page: the register window the guest maps at
/// 0xFEE00000 in xAPIC mode. Every local-APIC register access is an MMIO
/// exit against the same [`UserspaceLapic`] state the (intercepted)
/// TSC-deadline MSR touches.
pub const APIC_MMIO_BASE: u64 = 0xFEE0_0000;
/// The APIC MMIO page size: 4 KiB, one page.
pub const APIC_MMIO_SIZE: u64 = 0x1000;
/// The xAPIC register window: MMIO offset = register offset, 32-bit
/// little-endian; the x2APIC MSR index is `0x800 + offset / 16`.
const APIC_MMIO_STRIDE: u64 = 16;

/// The bus adapter exposing [`UserspaceLapic`] over its MMIO page. The
/// second field is the guest's current virtual TSC tick, refreshed by the
/// vCPU at every handled exit - the TMCCT countdown reads live state.
pub struct UserspaceLapicMmio(
    pub Arc<Mutex<UserspaceLapic>>,
    pub Arc<std::sync::atomic::AtomicU64>,
);

impl BusDevice for UserspaceLapicMmio {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        let mut lapic = self.0.lock().expect("userspace local APIC lock poisoned");
        let tsc_now = self
            .1
            .load(std::sync::atomic::Ordering::Relaxed);
        let index = MSR_X2APIC_FIRST + (offset / APIC_MMIO_STRIDE) as u32;
        let value = lapic.read_msr(index, tsc_now).unwrap_or(0) as u32;
        let bytes = value.to_le_bytes();
        for (index, byte) in bytes.iter().enumerate().take(data.len()) {
            data[index] = *byte;
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<std::sync::Barrier>> {
        let mut raw = [0u8; 4];
        for (index, byte) in data.iter().take(4).enumerate() {
            raw[index] = *byte;
        }
        let value = u64::from(u32::from_le_bytes(raw));
        let mut lapic = self.0.lock().expect("userspace local APIC lock poisoned");
        let tsc_now = self
            .1
            .load(std::sync::atomic::Ordering::Relaxed);
        let index = MSR_X2APIC_FIRST + (offset / APIC_MMIO_STRIDE) as u32;
        let _ = lapic.write_msr(index, value, tsc_now);
        None
    }
}

/// The guest's IOAPIC: the ACPI-declared MMIO page at 0xFEC00000. GSI
/// devices assert pins here and an unmasked entry hands back the vector
/// the guest programmed, for `KVM_INTERRUPT` delivery.
#[derive(Debug, Default)]
pub struct UserspaceIoapic {
    register_select: u8,
    redirections: [u64; 24],
}

impl UserspaceIoapic {
    /// The vector to inject when `pin` asserts, if its entry is
    /// unmasked and carries a vector.
    pub fn assert_line(&mut self, pin: u8) -> Option<u32> {
        let entry = *self.redirections.get(usize::from(pin))?;
        let vector = (entry & 0xFF) as u32;
        (entry & (1 << 16) == 0 && vector != 0).then_some(vector)
    }

    /// The RTE for `pin`, for the delivery bookkeeping.
    pub fn redirection(&self, pin: u8) -> Option<u64> {
        self.redirections.get(usize::from(pin)).copied()
    }
}

impl BusDevice for UserspaceIoapic {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        let value: u32 = match offset {
            IOREGSEL_OFFSET => u32::from(self.register_select),
            IOWIN_OFFSET => match self.register_select {
                0x00 => 0,
                0x01 => IOAPIC_VERSION,
                0x02 => 0,
                register if register >= IOAPIC_FIRST_REDIRECTION => {
                    let entry_index = (register - IOAPIC_FIRST_REDIRECTION) / 2;
                    let entry = self
                        .redirections
                        .get(usize::from(entry_index))
                        .copied()
                        .unwrap_or(0);
                    if (register - IOAPIC_FIRST_REDIRECTION) % 2 == 0 {
                        (entry & 0xFFFF_FFFF) as u32
                    } else {
                        (entry >> 32) as u32
                    }
                }
                _ => 0,
            },
            _ => 0,
        };
        let bytes = value.to_le_bytes();
        for (index, byte) in bytes.iter().enumerate().take(data.len()) {
            data[index] = *byte;
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<std::sync::Barrier>> {
        let mut raw = [0u8; 4];
        for (index, byte) in data.iter().take(4).enumerate() {
            raw[index] = *byte;
        }
        let value = u32::from_le_bytes(raw);
        match offset {
            IOREGSEL_OFFSET => self.register_select = value as u8,
            IOWIN_OFFSET => {
                if self.register_select >= IOAPIC_FIRST_REDIRECTION {
                    let entry_index =
                        (self.register_select - IOAPIC_FIRST_REDIRECTION) / 2;
                    if let Some(entry) = self.redirections.get_mut(usize::from(entry_index)) {
                        if (self.register_select - IOAPIC_FIRST_REDIRECTION) % 2 == 0 {
                            *entry =
                                (*entry & 0xFFFF_FFFF_0000_0000) | u64::from(value);
                        } else {
                            *entry = (*entry & 0xFFFF_FFFF) | (u64::from(value) << 32);
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsc_deadline_arms_and_fires_at_its_virtual_position() {
        let mut lapic = UserspaceLapic::default();
        // The kernel's LVT timer: vector 236, unmasked, one-shot mode.
        lapic.write_msr(MSR_X2APIC_LVT_TIMER, 236, 0).unwrap();
        // The guest arms 9.5ms ahead of its own TSC.
        lapic
            .write_msr(MSR_IA32_TSC_DEADLINE, 486_000_000, 476_000_000)
            .unwrap();
        assert_eq!(
            lapic.read_msr(MSR_IA32_TSC_DEADLINE, 476_000_000).unwrap(),
            486_000_000
        );
        // Not yet due.
        assert_eq!(lapic.timer_due(480_000_000), None);
        // Due: the vector, and the deadline disarms (one-shot).
        assert_eq!(lapic.timer_due(486_000_000), Some(236));
        assert_eq!(lapic.read_msr(MSR_IA32_TSC_DEADLINE, 486_000_000).unwrap(), 0);
        // The next boundary carries no spurious delivery.
        assert_eq!(lapic.timer_due(490_000_000), None);
    }

    #[test]
    fn zero_tsc_deadline_is_a_disarm() {
        let mut lapic = UserspaceLapic::default();
        lapic.write_msr(MSR_X2APIC_LVT_TIMER, 236, 0).unwrap();
        lapic.write_msr(MSR_IA32_TSC_DEADLINE, 1_000, 0).unwrap();
        lapic.write_msr(MSR_IA32_TSC_DEADLINE, 0, 500).unwrap();
        assert_eq!(lapic.timer_due(2_000), None);
    }

    #[test]
    fn tmict_periodic_re_arms_whole_periods() {
        let mut lapic = UserspaceLapic::default();
        // Periodic mode, vector 236: LVT mode bits (18:17) = 01.
        lapic
            .write_msr(MSR_X2APIC_LVT_TIMER, (1 << LVT_MODE_SHIFT) | 236, 0)
            .unwrap();
        // TDCR divide by 2, initial count 100: a 200-tsc-tick period.
        lapic.write_msr(MSR_X2APIC_TDCR, 0x0, 0).unwrap();
        lapic.write_msr(MSR_X2APIC_TMICT, 100, 1_000).unwrap();
        // The countdown reads in divided ticks.
        assert_eq!(lapic.read_msr(MSR_X2APIC_TMCCT, 1_050).unwrap(), 75);
        // First tick at 1200.
        assert_eq!(lapic.timer_due(1_100), None);
        assert_eq!(lapic.timer_due(1_200), Some(236));
        // Re-armed to 1400; a late boundary at 1900 advances one whole
        // period, not a burst.
        assert_eq!(lapic.timer_due(1_300), None);
        assert_eq!(lapic.timer_due(1_900), Some(236));
        assert_eq!(lapic.timer_due(2_000), None);
        assert_eq!(lapic.timer_due(2_100), Some(236));
    }

    #[test]
    fn masked_lvt_never_delivers() {
        let mut lapic = UserspaceLapic::default();
        lapic.write_msr(MSR_X2APIC_LVT_TIMER, LVT_MASKED | 236, 0).unwrap();
        lapic.write_msr(MSR_IA32_TSC_DEADLINE, 1_000, 0).unwrap();
        assert_eq!(lapic.timer_due(2_000), None);
    }

    #[test]
    fn ioapic_programs_and_delivers_vectors() {
        let mut ioapic = UserspaceIoapic::default();
        // The kernel selects the GSI 4 redirection's low half and writes
        // vector 0x34, edge, unmasked.
        ioapic.write(0, 0x00, &[0x12, 0, 0, 0]);
        ioapic.write(0, 0x10, &[0x34, 0, 0, 0]);
        ioapic.write(0, 0x00, &[0x13, 0, 0, 0]);
        ioapic.write(0, 0x10, &[0, 0, 0, 0]);
        assert_eq!(ioapic.assert_line(4), Some(0x34));
        // Reads round-trip through the select register.
        ioapic.write(0, 0x00, &[0x12, 0, 0, 0]);
        let mut out = [0u8; 4];
        ioapic.read(0, 0x10, &mut out);
        assert_eq!(out, [0x34, 0, 0, 0]);
        // The version register answers the probe.
        ioapic.write(0, 0x00, &[0x01, 0, 0, 0]);
        ioapic.read(0, 0x10, &mut out);
        assert_eq!(u32::from_le_bytes(out), IOAPIC_VERSION);
        // Masked entries stay silent.
        ioapic.write(0, 0x00, &[0x12, 0, 0, 0]);
        ioapic.write(0, 0x10, &[0x34, 0, 0x00, 0x01]);
        assert_eq!(ioapic.assert_line(4), None);
    }
}
