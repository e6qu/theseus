// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Virtual clock — the deterministic core of Track B′ (tick-stepped time).
//!
//! The model: guest-observable time does not follow the host wall clock.
//! Instead, the vCPU runs in bounded *quanta*; at each quantum boundary the
//! clock advances by exactly one `tick_ns`, regardless of how much host time
//! or guest work the quantum contained. Time is therefore a pure function of
//! the tick count, which is a pure function of the orchestrator's schedule —
//! identical on every replay of the same seed.
//!
//! This module is deliberately free of KVM calls (pure, fully testable); the
//! application of the virtual clock to the guest (kvmclock/TSC writes) lives
//! in `arch::x86_64` and is exercised only on KVM hosts.

use serde::{Deserialize, Serialize};

/// Default tick length: 1 ms of virtual time per quantum.
pub const DEFAULT_TICK_NS: u64 = 1_000_000;

/// A tick-stepped virtual clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualClock {
    /// Current virtual time, nanoseconds since boot.
    now_ns: u64,
    /// Virtual time advanced per quantum.
    tick_ns: u64,
    /// Number of quanta elapsed. `now_ns == tick_ns * tick_count` always.
    tick_count: u64,
}

/// Serializable state for snapshots/branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualClockState {
    /// Current virtual time, nanoseconds since boot.
    pub now_ns: u64,
    /// Virtual time advanced per quantum.
    pub tick_ns: u64,
    /// Number of quanta elapsed.
    pub tick_count: u64,
}

impl VirtualClock {
    /// A clock at time zero with the given tick length.
    pub fn new(tick_ns: u64) -> Self {
        assert!(tick_ns > 0, "tick must be non-zero");
        VirtualClock {
            now_ns: 0,
            tick_ns,
            tick_count: 0,
        }
    }

    /// Advance exactly one tick. Called at each quantum boundary.
    pub fn advance(&mut self) {
        self.tick_count += 1;
        self.now_ns += self.tick_ns;
    }

    /// Current virtual time in nanoseconds.
    pub fn now_ns(&self) -> u64 {
        self.now_ns
    }

    /// Quanta elapsed.
    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }

    /// Tick length in nanoseconds.
    pub fn tick_ns(&self) -> u64 {
        self.tick_ns
    }

    /// The counter value consistent with `now_ns` at the given frequency.
    ///
    /// Used for both x86_64 TSC (freq in kHz) and aarch64 CNTVCT (freq in Hz)
    /// writes when applying the virtual clock at a quantum boundary.
    pub fn ticks_for_time(now_ns: u64, freq_hz: u64) -> u64 {
        // now_ns * freq_hz / 1e9, computed in u128 to avoid overflow.
        u64::try_from((u128::from(now_ns) * u128::from(freq_hz)) / 1_000_000_000)
            .expect("virtual time overflow in counter conversion")
    }

    /// The TSC value consistent with `now_ns` at the given frequency.
    ///
    /// When applying the virtual clock to a guest, the vCPU's TSC must be set
    /// to this value *before* `KVM_SET_CLOCK`, so that kvmclock's
    /// TSC-to-nanoseconds mapping stays consistent.
    pub fn tsc_value(now_ns: u64, tsc_khz: u32) -> u64 {
        Self::ticks_for_time(now_ns, u64::from(tsc_khz) * 1_000)
    }

    /// Snapshot the clock state.
    pub fn save(&self) -> VirtualClockState {
        VirtualClockState {
            now_ns: self.now_ns,
            tick_ns: self.tick_ns,
            tick_count: self.tick_count,
        }
    }

    /// Restore from snapshotted state. Panics if the state is inconsistent
    /// (`now_ns != tick_ns * tick_count`).
    pub fn restore(state: &VirtualClockState) -> Self {
        assert!(
            state.now_ns == state.tick_ns * state.tick_count,
            "inconsistent virtual clock state"
        );
        VirtualClock {
            now_ns: state.now_ns,
            tick_ns: state.tick_ns,
            tick_count: state.tick_count,
        }
    }
}

impl Default for VirtualClock {
    fn default() -> Self {
        Self::new(DEFAULT_TICK_NS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_advance_is_tick_driven() {
        let mut clock = VirtualClock::new(1_000_000);
        assert_eq!(clock.now_ns(), 0);
        for i in 1..=1000u64 {
            clock.advance();
            assert_eq!(clock.now_ns(), i * 1_000_000);
            assert_eq!(clock.tick_count(), i);
        }
    }

    #[test]
    fn test_time_is_pure_function_of_ticks() {
        // The determinism invariant, stated as a test: two clocks advanced the
        // same number of times agree, regardless of anything else.
        let mut a = VirtualClock::default();
        let mut b = VirtualClock::default();
        for _ in 0..777 {
            a.advance();
            b.advance();
        }
        assert_eq!(a, b);
    }

    #[test]
    fn test_tsc_value() {
        // 1 second at 2.5 GHz = 2_500_000_000 ticks.
        assert_eq!(VirtualClock::tsc_value(1_000_000_000, 2_500_000), 2_500_000_000);
        // Zero time is zero TSC.
        assert_eq!(VirtualClock::tsc_value(0, 2_500_000), 0);
        // Large uptimes don't overflow the u128 intermediate: ~100 years at
        // 3 GHz fits in u64 TSC ticks (u64 maxes out around ~195 years).
        let year_ns = 365 * 24 * 3600 * 1_000_000_000u64;
        let tsc = VirtualClock::tsc_value(100 * year_ns, 3_000_000);
        assert_eq!(tsc, 100 * year_ns / 1_000_000 * 3_000_000);
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut clock = VirtualClock::new(250_000);
        for _ in 0..42 {
            clock.advance();
        }
        let state = clock.save();
        let bytes = bitcode::serialize(&state).unwrap();
        let restored_state: VirtualClockState = bitcode::deserialize(&bytes).unwrap();
        let restored = VirtualClock::restore(&restored_state);
        assert_eq!(clock, restored);
    }

    #[test]
    #[should_panic]
    fn test_restore_rejects_inconsistent_state() {
        let bad = VirtualClockState {
            now_ns: 999,
            tick_ns: 1000,
            tick_count: 1,
        };
        let _ = VirtualClock::restore(&bad);
    }

    #[test]
    #[should_panic]
    fn test_zero_tick_rejected() {
        let _ = VirtualClock::new(0);
    }
}
