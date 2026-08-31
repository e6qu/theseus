// Copyright 2026 Theseus contributors.
// SPDX-License-Identifier: Apache-2.0

//! Simulated network backend — deterministic, host-independent packet I/O.
//!
//! Replaces the host tap device behind the virtio-net frontend. The guest sees
//! a normal NIC; frames never touch the host network. v1 provides the three
//! fault/injection primitives the deterministic environment needs:
//!
//! - **loopback**: TX frames are queued back for RX delivery (driver bring-up
//!   without any host networking)
//! - **partition**: all traffic is dropped in both directions
//! - **deterministic random drops**: per-frame drops driven by a seeded ChaCha
//!   stream, so a given seed + frame sequence always produces the same drops
//!
//! Frame *delay* is deliberately omitted for v1: it needs virtual time
//! (Phase 3) rather than host timers.

use std::collections::VecDeque;

use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};

/// Configuration for a simulated network backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimNetConfig {
    /// Seed for the deterministic drop stream (default 0).
    #[serde(default)]
    pub seed: u64,
    /// Loop TX frames back into the RX path.
    #[serde(default)]
    pub loopback: bool,
    /// Drop probability, parts per million, applied to every frame (both
    /// directions), driven by the seeded RNG. 0 = never drop.
    #[serde(default)]
    pub drop_ppm: u32,
    /// Simulate a total network partition: drop everything.
    #[serde(default)]
    pub partitioned: bool,
}

impl Default for SimNetConfig {
    fn default() -> Self {
        SimNetConfig {
            seed: 0,
            loopback: true,
            drop_ppm: 0,
            partitioned: false,
        }
    }
}

/// The simulated backend. Pure safe Rust; the only entropy source is the
/// seeded RNG.
#[derive(Debug)]
pub struct SimNet {
    config: SimNetConfig,
    /// Frames awaiting RX delivery to the guest.
    rx_queue: VecDeque<Vec<u8>>,
    rng: ChaCha8Rng,
    /// Frames accepted from the guest TX path.
    pub tx_frames: u64,
    /// Frames delivered to the guest RX path.
    pub rx_frames: u64,
    /// Frames dropped by partition or the drop stream.
    pub dropped: u64,
}

impl SimNet {
    pub fn new(config: SimNetConfig) -> Self {
        SimNet {
            config,
            rx_queue: VecDeque::new(),
            rng: ChaCha8Rng::seed_from_u64(config.seed),
            tx_frames: 0,
            rx_frames: 0,
            dropped: 0,
        }
    }

    pub fn config(&self) -> SimNetConfig {
        self.config
    }

    /// Deterministic per-frame drop decision.
    fn should_drop(&mut self) -> bool {
        if self.config.partitioned {
            return true;
        }
        if self.config.drop_ppm == 0 {
            return false;
        }
        self.rng.next_u32() % 1_000_000 < self.config.drop_ppm
    }

    /// Accept a frame from the guest TX path.
    pub fn write_frame(&mut self, frame: &[u8]) {
        self.tx_frames += 1;
        if self.should_drop() {
            self.dropped += 1;
            return;
        }
        if self.config.loopback {
            self.rx_queue.push_back(frame.to_vec());
        }
    }

    /// True when a frame is waiting for RX delivery.
    pub fn has_pending_rx(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    /// Pop the next frame for the guest RX path. Returns frame length.
    pub fn read_frame(&mut self, buf: &mut [u8]) -> Option<usize> {
        let frame = self.rx_queue.pop_front()?;
        if frame.len() > buf.len() {
            // Truncating would silently corrupt; drop instead, deterministically.
            self.dropped += 1;
            return None;
        }
        buf[..frame.len()].copy_from_slice(&frame);
        self.rx_frames += 1;
        Some(frame.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loopback_roundtrip() {
        let mut sim = SimNet::new(SimNetConfig::default());
        let frame = [0xAA; 64];
        sim.write_frame(&frame);
        assert!(sim.has_pending_rx());

        let mut buf = [0u8; 128];
        let len = sim.read_frame(&mut buf).unwrap();
        assert_eq!(len, 64);
        assert_eq!(&buf[..64], &frame);
        assert!(!sim.has_pending_rx());
        assert_eq!(sim.tx_frames, 1);
        assert_eq!(sim.rx_frames, 1);
        assert_eq!(sim.dropped, 0);
    }

    #[test]
    fn test_partition_drops_everything() {
        let mut sim = SimNet::new(SimNetConfig {
            partitioned: true,
            ..Default::default()
        });
        sim.write_frame(&[1; 64]);
        assert!(!sim.has_pending_rx());
        assert_eq!(sim.dropped, 1);
    }

    #[test]
    fn test_drops_are_deterministic() {
        let cfg = SimNetConfig {
            seed: 1234,
            loopback: true,
            drop_ppm: 500_000, // drop ~half
            partitioned: false,
        };
        let run = || {
            let mut sim = SimNet::new(cfg);
            let mut delivered = Vec::new();
            for i in 0..100u8 {
                sim.write_frame(&[i; 32]);
                let mut buf = [0u8; 64];
                while let Some(len) = sim.read_frame(&mut buf) {
                    delivered.push(buf[..len].to_vec());
                }
            }
            (delivered, sim.dropped)
        };
        let (d1, dropped1) = run();
        let (d2, dropped2) = run();
        assert_eq!(d1, d2, "same seed must produce the same drop pattern");
        assert_eq!(dropped1, dropped2);
        assert!(dropped1 > 0, "drop_ppm=50% should drop some of 100 frames");
        assert!(dropped1 < 100, "drop_ppm=50% should deliver some frames");
    }

    #[test]
    fn test_oversized_rx_buffer_underrun_drops() {
        let mut sim = SimNet::new(SimNetConfig::default());
        sim.write_frame(&[0xAB; 128]);
        let mut small = [0u8; 16];
        // Frame doesn't fit: dropped deterministically, not truncated.
        assert_eq!(sim.read_frame(&mut small), None);
        assert_eq!(sim.dropped, 1);
        assert!(!sim.has_pending_rx());
    }
}
