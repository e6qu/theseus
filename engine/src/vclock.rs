// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Virtual clock — the deterministic core of Track B′ (tick-stepped time).
//!
//! The model: guest-observable time does not follow the host wall clock.
//! Instead, the vCPU runs in bounded *quanta*; at each quantum boundary the
//! clock advances by exactly one `tick_ns`, regardless of how much host time
//! or guest work the quantum contained. Time is therefore a pure function of
//! the tick count, which is a pure function of the orchestrator's schedule —
//! identical when the same tick/jump sequence is replayed. A seed alone does
//! not control the exits or arbitrary guest instruction ordering.
//!
//! This module is deliberately free of KVM calls (pure, fully testable); the
//! application of the virtual clock to the guest (kvmclock/TSC writes) lives
//! in the architecture-specific VMM adapters and is exercised only on KVM hosts.

use serde::{Deserialize, Serialize};

/// Default tick length: 1 ms of virtual time per quantum.
pub const DEFAULT_TICK_NS: u64 = 1_000_000;

/// Maximum guest-clock rate multiplier while a recorded clock-rate fault is
/// active.
pub const MAX_CLOCK_RATE: u32 = 16;

/// One whole rate multiplier in milli-units: rates are represented in
/// thousandths internally so sub-1x slowdowns (0.1x–0.9x = 100–900) share
/// the same arithmetic as whole multipliers (1000–16000).
pub const RATE_MILLI_ONE: u32 = 1000;

/// The slowest sub-1x rate in milli-units.
pub const MIN_CLOCK_RATE_MILLI: u32 = 100;

/// The fastest rate in milli-units.
pub const MAX_CLOCK_RATE_MILLI: u32 = MAX_CLOCK_RATE * RATE_MILLI_ONE;

/// A tick-stepped virtual clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualClock {
    /// Current virtual time, nanoseconds since boot.
    now_ns: u64,
    /// Virtual time advanced per quantum at rate 1.
    tick_ns: u64,
    /// Number of normal quanta elapsed. Explicit clock jumps do not change it.
    tick_count: u64,
    /// Guest-clock rate applied at every quantum boundary, in milli-units
    /// (1000 = 1x, 100..900 = 0.1x..0.9x).
    rate_milli: u32,
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
    /// Guest-clock rate multiplier. Snapshots taken before rate faults omit
    /// it, which means rate 1.
    #[serde(default = "default_rate")]
    pub rate: u32,
    /// The rate in milli-units when a sub-1x (or explicitly recorded)
    /// window applies. When absent, `rate` whole multipliers apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_milli: Option<u32>,
}

fn default_rate() -> u32 {
    1
}

impl VirtualClock {
    /// A clock at time zero with the given tick length.
    pub fn new(tick_ns: u64) -> Self {
        assert!(tick_ns > 0, "tick must be non-zero");
        VirtualClock {
            now_ns: 0,
            tick_ns,
            tick_count: 0,
            rate_milli: RATE_MILLI_ONE,
        }
    }

    /// The guest-clock rate applied at every quantum boundary, in
    /// milli-units.
    pub fn rate_milli(&self) -> u32 {
        self.rate_milli
    }

    /// Set the guest-clock rate for later quanta, in milli-units: 1000 is
    /// 1x, 100..900 are sub-1x slowdowns, up to the 16x maximum.
    pub fn set_rate_milli(&mut self, rate_milli: u32) {
        assert!(
            (MIN_CLOCK_RATE_MILLI..=MAX_CLOCK_RATE_MILLI)
                .contains(&rate_milli),
            "clock rate must be between {} and {} milli-units",
            MIN_CLOCK_RATE_MILLI,
            MAX_CLOCK_RATE_MILLI
        );
        self.rate_milli = rate_milli;
    }

    /// Advance exactly one quantum at the current rate. Called at each
    /// quantum boundary.
    pub fn advance(&mut self) {
        self.tick_count += 1;
        // Milli-units keep whole multipliers byte-stable (rate_milli/1000
        // == rate for 1000..16000) while sub-1x rates floor per quantum,
        // still a pure function of the tick count.
        self.now_ns += self.tick_ns * u64::from(self.rate_milli) / 1000;
    }

    /// Move time forward or backward without adding a scheduling quantum.
    ///
    /// This is for an explicit, recorded fault only. Normal execution must
    /// use [`Self::advance`]. A negative delta moves the clock backward, the
    /// way an NTP correction does, but never before the anchored tick count:
    /// the saved/restore invariant `now_ns >= tick_ns * tick_count` holds on
    /// every path.
    pub fn jump(&mut self, delta_ns: i64) {
        assert!(delta_ns != 0, "clock jump must be non-zero");
        if delta_ns > 0 {
            self.now_ns = self
                .now_ns
                .checked_add(delta_ns as u64)
                .expect("virtual time overflow during clock jump");
        } else {
            // Backward jumps saturate at the anchored tick count instead of
            // failing the run: the floor keeps the saved/restore invariant
            // `now_ns >= tick_ns * tick_count` intact on every path.
            // The slowest supported rate bounds the minimum: backward
            // jumps saturate there instead of rewinding elapsed quanta.
            let floor = u64::try_from(
                u128::from(self.tick_ns)
                    * u128::from(self.tick_count)
                    * u128::from(MIN_CLOCK_RATE_MILLI)
                    / 1000,
            )
            .unwrap_or(u64::MAX);
            self.now_ns = self
                .now_ns
                .saturating_sub(delta_ns.unsigned_abs())
                .max(floor);
        }
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
            rate: self.rate_milli / RATE_MILLI_ONE,
            rate_milli: Some(self.rate_milli),
        }
    }

    /// Restore from snapshotted state.
    ///
    /// `now_ns` may be ahead of `tick_ns * tick_count` when a recorded clock
    /// jump was applied before the snapshot.
    pub fn restore(state: &VirtualClockState) -> Self {
        // The slowest supported rate bounds how little time N quanta can
        // add; the floor rejects grossly corrupt snapshots without
        // assuming which rate window applied at each quantum.
        let floor = u128::from(state.tick_ns)
            * u128::from(state.tick_count)
            * u128::from(MIN_CLOCK_RATE_MILLI)
            / 1000;
        assert!(
            state.tick_ns > 0 && u128::from(state.now_ns) >= floor,
            "inconsistent virtual clock state"
        );
        VirtualClock {
            now_ns: state.now_ns,
            tick_ns: state.tick_ns,
            tick_count: state.tick_count,
            rate_milli: state.rate_milli.unwrap_or(state.rate * RATE_MILLI_ONE),
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
        assert_eq!(
            VirtualClock::tsc_value(1_000_000_000, 2_500_000),
            2_500_000_000
        );
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
            now_ns: 99,
            tick_ns: 1000,
            tick_count: 1,
            rate: 1,
            rate_milli: None,
        };
        let _ = VirtualClock::restore(&bad);
    }

    #[test]
    fn test_clock_jump_does_not_add_a_quantum() {
        let mut clock = VirtualClock::new(1_000);
        clock.advance();
        clock.jump(10_000);
        assert_eq!(clock.now_ns(), 11_000);
        assert_eq!(clock.tick_count(), 1);
        assert_eq!(VirtualClock::restore(&clock.save()), clock);
    }

    #[test]
    fn test_clock_rate_multiplies_later_quanta_and_survives_round_trips() {
        let mut clock = VirtualClock::new(1_000);
        clock.advance();
        clock.set_rate_milli(4_000);
        clock.advance();
        assert_eq!(clock.now_ns(), 5_000);
        assert_eq!(clock.tick_count(), 2);
        assert_eq!(VirtualClock::restore(&clock.save()), clock);
        clock.set_rate_milli(1_000);
        clock.advance();
        assert_eq!(clock.now_ns(), 6_000);
    }

    #[test]
    fn test_legacy_clock_state_restores_at_rate_one() {
        let state = VirtualClockState {
            now_ns: 2_000,
            tick_ns: 1_000,
            tick_count: 2,
            rate: 1,
            rate_milli: None,
        };
        assert_eq!(VirtualClock::restore(&state).rate_milli(), 1000);
    }

    #[test]
    fn test_sub_one_rate_advances_half_ticks_deterministically() {
        let mut clock = VirtualClock::new(1_000);
        clock.set_rate_milli(500);
        clock.advance();
        clock.advance();
        clock.advance();
        assert_eq!(clock.now_ns(), 1_500);
        assert_eq!(clock.tick_count(), 3);
        // Round-trips through the state with the milli rate intact.
        assert_eq!(
            VirtualClock::restore(&clock.save()).rate_milli(),
            500
        );
        clock.advance();
        assert_eq!(clock.now_ns(), 2_000);

        // Sub-1x snapshots restore: now_ns sits below the rate-1 tick
        // floor by design, and the restore floor scales with the rate.
        let state = clock.save();
        assert_eq!(VirtualClock::restore(&state), clock);
    }

    #[test]
    #[should_panic]
    fn test_sub_one_rate_bounds_rejected() {
        let mut clock = VirtualClock::new(1_000);
        clock.set_rate_milli(50);
    }

    #[test]
    #[should_panic]
    fn test_rate_bounds_rejected() {
        let mut clock = VirtualClock::new(1_000);
        clock.set_rate_milli(17_000);
    }

    #[test]
    fn test_clock_jump_moves_backward_above_the_tick_floor() {
        let mut clock = VirtualClock::new(1_000);
        clock.advance();
        clock.advance();
        clock.jump(50_000);
        clock.jump(-30_000);
        assert_eq!(clock.now_ns(), 22_000);
        assert_eq!(clock.tick_count(), 2);
        assert_eq!(VirtualClock::restore(&clock.save()), clock);
        // The floor is the slowest-rate bound: 2 ticks at 0.1x =
        // 2 * 1000 ns * 100 / 1000 = 200 ns. Backward jumps saturate
        // there instead of rewinding elapsed quanta.
        clock.jump(-20_000);
        assert_eq!(clock.now_ns(), 2_000);
        clock.jump(-1_900);
        assert_eq!(clock.now_ns(), 200, "floor clamps the backward jump");
        clock.jump(-1);
        assert_eq!(clock.now_ns(), 200, "floor clamps the backward jump");
        assert_eq!(VirtualClock::restore(&clock.save()), clock);
    }

    #[test]
    #[should_panic]
    fn test_zero_jump_rejected() {
        let mut clock = VirtualClock::new(1_000);
        clock.jump(0);
    }

    #[test]
    #[should_panic]
    fn test_zero_tick_rejected() {
        let _ = VirtualClock::new(0);
    }
}
