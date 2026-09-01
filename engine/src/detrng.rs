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
//! A VM owns a [`Stream`]. Code running on its host-side execution path enters
//! that stream with [`with_stream`], so concurrent timelines never interleave
//! guest-visible host randomness. The process stream remains only as a safe,
//! deterministic fallback for legacy callers outside a VM context.
//!
//! Default seed is 0: in this fork, deterministic is the default, and
//! randomized runs are the opt-in.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, MutexGuard};

use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

static STREAM: Mutex<Option<ChaCha8Rng>> = Mutex::new(None);

thread_local! {
    static CURRENT: RefCell<Option<Stream>> = const { RefCell::new(None) };
}

/// A deterministic host-side RNG owned by one microVM timeline.
#[derive(Clone, Debug)]
pub struct Stream(Arc<Mutex<ChaCha8Rng>>);

impl Stream {
    /// Start a fresh stream from `seed`.
    pub fn seeded(seed: u64) -> Self {
        Self(Arc::new(Mutex::new(ChaCha8Rng::seed_from_u64(seed))))
    }
}

struct StreamScope(Option<Stream>);

impl Drop for StreamScope {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            current.replace(self.0.take());
        });
    }
}

/// Run `operation` with `stream` as the current VM's host-side RNG.
pub fn with_stream<T>(stream: &Stream, operation: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|current| current.replace(Some(stream.clone())));
    let restore = StreamScope(previous);
    let result = operation();
    drop(restore);
    result
}

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
    if let Some(stream) = CURRENT.with(|current| current.borrow().clone()) {
        stream.0.lock().expect("Poisoned lock").fill_bytes(buf);
        return;
    }
    lock_stream().as_mut().unwrap().fill_bytes(buf);
}

/// Next deterministic u32.
pub fn next_u32() -> u32 {
    if let Some(stream) = CURRENT.with(|current| current.borrow().clone()) {
        return stream.0.lock().expect("Poisoned lock").next_u32();
    }
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

    #[test]
    fn streams_are_isolated_when_their_calls_interleave() {
        let left = Stream::seeded(42);
        let right = Stream::seeded(1337);
        let mut left_a = [0; 16];
        let mut right_a = [0; 16];
        with_stream(&left, || fill_bytes(&mut left_a));
        with_stream(&right, || fill_bytes(&mut right_a));

        let mut left_b = [0; 16];
        let mut right_b = [0; 16];
        with_stream(&left, || fill_bytes(&mut left_b));
        with_stream(&right, || fill_bytes(&mut right_b));

        let expected = |seed| {
            let mut rng = ChaCha8Rng::seed_from_u64(seed);
            let mut first = [0; 16];
            let mut second = [0; 16];
            rng.fill_bytes(&mut first);
            rng.fill_bytes(&mut second);
            (first, second)
        };
        assert_eq!((left_a, left_b), expected(42));
        assert_eq!((right_a, right_b), expected(1337));
    }
}
