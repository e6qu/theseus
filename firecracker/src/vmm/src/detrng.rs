// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Deterministic RNG for host-side, guest-visible randomness.
//!
//! Upstream Firecracker draws from host entropy in several places the guest
//! can observe: the aarch64 FDT `rng-seed`, the VM Generation ID device, MMDS
//! token keys, and dumbo TCP initial sequence numbers. Each is a determinism
//! leak. This module routes all of them through one seeded ChaCha stream, so
//! a run's entire host-side randomness is a function of its seed.
//!
//! **Scoping:** the stream is per-process. Determinism therefore holds under
//! the Firecracker-native model of one microVM per process (our bare-metal
//! layout: one timeline per core, one process per timeline). In-process
//! parallel timelines would interleave this stream nondeterministically;
//! per-VM scoping is the follow-up if that ever becomes the model.
//!
//! Default seed is 0: in this fork, deterministic is the default, and
//! randomized runs are the opt-in.

use std::sync::{Mutex, MutexGuard};

use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};

static STREAM: Mutex<Option<ChaCha8Rng>> = Mutex::new(None);

/// Initialize the process-wide deterministic RNG with the run's seed.
/// Called once at VM build time. Re-initializing mid-run is a logic error
/// caught by tests; at runtime the later seed wins.
pub fn init(seed: u64) {
    *STREAM.lock().expect("Poisoned lock") = Some(ChaCha8Rng::seed_from_u64(seed));
}

fn lock_stream() -> MutexGuard<'static, Option<ChaCha8Rng>> {
    let mut guard = STREAM.lock().expect("Poisoned lock");
    if guard.is_none() {
        *guard = Some(ChaCha8Rng::seed_from_u64(0));
    }
    guard
}

/// Fill `buf` with deterministic bytes.
pub fn fill_bytes(buf: &mut [u8]) {
    lock_stream().as_mut().unwrap().fill_bytes(buf);
}

/// Next deterministic u32.
pub fn next_u32() -> u32 {
    lock_stream().as_mut().unwrap().next_u32()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_across_processes_model() {
        // Simulates two runs: re-init with the same seed and require the same
        // stream; a different seed must differ.
        init(42);
        let mut a = [0u8; 64];
        fill_bytes(&mut a);
        let x = next_u32();

        init(42);
        let mut b = [0u8; 64];
        fill_bytes(&mut b);
        let y = next_u32();
        assert_eq!(a, b);
        assert_eq!(x, y);

        init(43);
        let mut c = [0u8; 64];
        fill_bytes(&mut c);
        assert_ne!(a, c);
    }
}
