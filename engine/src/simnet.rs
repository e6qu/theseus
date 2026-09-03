// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

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
//! Frame delay and jitter advance in deterministic runner rounds, never host
//! time.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
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
    /// Scheduler rounds a delivered frame waits before the guest can receive it.
    #[serde(default)]
    pub latency_rounds: u32,
    /// Extra scheduler rounds chosen per delivered frame from the seeded RNG.
    /// A later frame can therefore arrive before an earlier one.
    #[serde(default)]
    pub jitter_rounds: u32,
}

impl Default for SimNetConfig {
    fn default() -> Self {
        SimNetConfig {
            seed: 0,
            loopback: true,
            drop_ppm: 0,
            partitioned: false,
            latency_rounds: 0,
            jitter_rounds: 0,
        }
    }
}

/// Deterministic frame counters collected by a simulated NIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimNetStats {
    /// Frames accepted from the guest TX path.
    pub tx_frames: u64,
    /// Frames delivered to the guest RX path.
    pub rx_frames: u64,
    /// Frames dropped by the simulated backend.
    pub dropped: u64,
}

#[derive(Debug)]
struct PendingFrame {
    ready_round: u64,
    sequence: u64,
    bytes: Vec<u8>,
}

/// The simulated backend. Pure safe Rust; the only entropy source is the
/// seeded RNG.
#[derive(Debug)]
pub struct SimNet {
    config: SimNetConfig,
    /// Frames awaiting RX delivery to the guest.
    rx_queue: VecDeque<PendingFrame>,
    round: u64,
    next_frame: u64,
    rng: ChaCha8Rng,
    /// Frames accepted from the guest TX path.
    pub tx_frames: u64,
    /// Frames delivered to the guest RX path.
    pub rx_frames: u64,
    /// Frames dropped by partition or the drop stream.
    pub dropped: u64,
    /// A shared deterministic L2 switch for a multi-guest topology. `None`
    /// retains the single-guest loopback backend used by the Firecracker API.
    switch: Option<SharedSimSwitch>,
    endpoint: Option<String>,
}

/// A topology-owned deterministic Ethernet switch.
///
/// Ports are named so membership and fan-out order are stable. The caller
/// drives VMs in a deterministic order; this switch then preserves the exact
/// order in which those VMs submit frames, without host sockets or host
/// networking.
#[derive(Debug, Default)]
pub struct SimSwitch {
    ports: BTreeMap<String, VecDeque<PendingFrame>>,
    round: u64,
    next_frame: u64,
}

/// Shared ownership used by all simulated NICs in one topology runner.
pub type SharedSimSwitch = Arc<Mutex<SimSwitch>>;

/// An error while attaching a NIC to a deterministic switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimSwitchError {
    /// Port names must be non-empty and unique within their switch.
    InvalidPort(String),
    /// A second NIC attempted to attach using the same port name.
    DuplicatePort(String),
}

impl std::fmt::Display for SimSwitchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPort(port) => write!(formatter, "invalid simulated switch port {port:?}"),
            Self::DuplicatePort(port) => {
                write!(formatter, "simulated switch port {port:?} already exists")
            }
        }
    }
}

impl std::error::Error for SimSwitchError {}

impl SimSwitch {
    /// Make an empty topology switch.
    pub fn new() -> Self {
        Self::default()
    }

    fn attach(&mut self, port: &str) -> Result<(), SimSwitchError> {
        if port.is_empty() {
            return Err(SimSwitchError::InvalidPort(port.to_owned()));
        }
        if self.ports.contains_key(port) {
            return Err(SimSwitchError::DuplicatePort(port.to_owned()));
        }
        self.ports.insert(port.to_owned(), VecDeque::new());
        Ok(())
    }

    fn detach(&mut self, port: &str) {
        self.ports.remove(port);
    }

    fn deliver(&mut self, source: &str, include_source: bool, delay_rounds: u32, frame: &[u8]) {
        let ready_round = self.round.saturating_add(u64::from(delay_rounds));
        let sequence = self.next_frame;
        self.next_frame = self.next_frame.saturating_add(1);
        for (port, queue) in &mut self.ports {
            if include_source || port != source {
                push_pending(
                    queue,
                    PendingFrame {
                        ready_round,
                        sequence,
                        bytes: frame.to_vec(),
                    },
                );
            }
        }
    }

    fn receive(&mut self, port: &str) -> Option<Vec<u8>> {
        let queue = self.ports.get_mut(port)?;
        (queue.front()?.ready_round <= self.round).then(|| queue.pop_front().unwrap().bytes)
    }

    fn has_pending_rx(&self, port: &str) -> bool {
        self.ports
            .get(port)
            .and_then(VecDeque::front)
            .is_some_and(|frame| frame.ready_round <= self.round)
    }

    /// Advance the deterministic topology scheduler by one round.
    pub fn advance_round(&mut self) {
        self.round = self.round.saturating_add(1);
    }

    /// Return sorted port names for a replayable topology fingerprint.
    pub fn ports(&self) -> Vec<String> {
        self.ports.keys().cloned().collect()
    }
}

/// Keep each RX queue ordered by deterministic delivery time, with submission
/// order as a stable tiebreaker. This permits configured jitter to reorder
/// frames without involving host scheduling.
fn push_pending(queue: &mut VecDeque<PendingFrame>, frame: PendingFrame) {
    let position = queue
        .iter()
        .position(|queued| {
            (queued.ready_round, queued.sequence) > (frame.ready_round, frame.sequence)
        })
        .unwrap_or(queue.len());
    queue.insert(position, frame);
}

impl SimNet {
    pub fn new(config: SimNetConfig) -> Self {
        SimNet {
            config,
            rx_queue: VecDeque::new(),
            round: 0,
            next_frame: 0,
            rng: ChaCha8Rng::seed_from_u64(config.seed),
            tx_frames: 0,
            rx_frames: 0,
            dropped: 0,
            switch: None,
            endpoint: None,
        }
    }

    /// Create a NIC attached to a topology-owned deterministic switch.
    ///
    /// Every endpoint must have a unique, stable name (the Compose runner
    /// uses `network/service`). The switch is in-process by design: it avoids
    /// host packet scheduling and makes delivery part of the replay timeline.
    pub fn new_with_switch(
        config: SimNetConfig,
        switch: SharedSimSwitch,
        endpoint: impl Into<String>,
    ) -> Result<Self, SimSwitchError> {
        let endpoint = endpoint.into();
        switch
            .lock()
            .expect("simulated switch lock poisoned")
            .attach(&endpoint)?;
        Ok(SimNet {
            config,
            rx_queue: VecDeque::new(),
            round: 0,
            next_frame: 0,
            rng: ChaCha8Rng::seed_from_u64(config.seed),
            tx_frames: 0,
            rx_frames: 0,
            dropped: 0,
            switch: Some(switch),
            endpoint: Some(endpoint),
        })
    }

    pub fn config(&self) -> SimNetConfig {
        self.config
    }

    /// Return deterministic frame counters for this NIC.
    pub fn stats(&self) -> SimNetStats {
        SimNetStats {
            tx_frames: self.tx_frames,
            rx_frames: self.rx_frames,
            dropped: self.dropped,
        }
    }

    /// Advance a standalone simulated NIC by one deterministic runner round.
    ///
    /// Topology-owned NICs share a switch, so their runner advances that switch
    /// exactly once after every service has been pumped.
    pub fn advance_round(&mut self) {
        if self.switch.is_none() {
            self.round = self.round.saturating_add(1);
        }
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

    /// The base delay plus a seeded, per-frame jitter. Use a u64 modulus so
    /// the full u32 configuration range remains valid.
    fn delivery_delay_rounds(&mut self) -> u32 {
        let jitter =
            u32::try_from(self.rng.next_u64() % (u64::from(self.config.jitter_rounds) + 1))
                .expect("configured jitter is bounded by u32");
        self.config.latency_rounds.saturating_add(jitter)
    }

    /// Accept a frame from the guest TX path.
    pub fn write_frame(&mut self, frame: &[u8]) {
        self.tx_frames += 1;
        if self.should_drop() {
            self.dropped += 1;
            return;
        }
        let delay_rounds = self.delivery_delay_rounds();
        if let (Some(switch), Some(endpoint)) = (&self.switch, &self.endpoint) {
            switch
                .lock()
                .expect("simulated switch lock poisoned")
                .deliver(endpoint, self.config.loopback, delay_rounds, frame);
        } else if self.config.loopback {
            let frame = PendingFrame {
                ready_round: self.round.saturating_add(u64::from(delay_rounds)),
                sequence: self.next_frame,
                bytes: frame.to_vec(),
            };
            self.next_frame = self.next_frame.saturating_add(1);
            push_pending(&mut self.rx_queue, frame);
        }
    }

    /// True when a frame is waiting for RX delivery.
    pub fn has_pending_rx(&self) -> bool {
        if let (Some(switch), Some(endpoint)) = (&self.switch, &self.endpoint) {
            return switch
                .lock()
                .expect("simulated switch lock poisoned")
                .has_pending_rx(endpoint);
        }
        self.rx_queue
            .front()
            .is_some_and(|frame| frame.ready_round <= self.round)
    }

    /// Pop the next frame for the guest RX path. Returns frame length.
    pub fn read_frame(&mut self, buf: &mut [u8]) -> Option<usize> {
        let frame = if let (Some(switch), Some(endpoint)) = (&self.switch, &self.endpoint) {
            switch
                .lock()
                .expect("simulated switch lock poisoned")
                .receive(endpoint)?
        } else {
            if self.rx_queue.front()?.ready_round > self.round {
                return None;
            }
            self.rx_queue.pop_front()?.bytes
        };
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

impl Drop for SimNet {
    fn drop(&mut self) {
        if let (Some(switch), Some(endpoint)) = (&self.switch, &self.endpoint) {
            switch
                .lock()
                .expect("simulated switch lock poisoned")
                .detach(endpoint);
        }
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
        assert_eq!(
            sim.stats(),
            SimNetStats {
                tx_frames: 1,
                rx_frames: 1,
                dropped: 0,
            }
        );
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
    fn test_delay_waits_for_deterministic_rounds() {
        let mut sim = SimNet::new(SimNetConfig {
            latency_rounds: 2,
            ..Default::default()
        });
        sim.write_frame(b"delayed");
        assert!(!sim.has_pending_rx());
        sim.advance_round();
        assert!(!sim.has_pending_rx());
        sim.advance_round();
        assert!(sim.has_pending_rx());
        let mut buffer = [0; 16];
        let length = sim.read_frame(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"delayed");
    }

    #[test]
    fn test_jitter_is_seeded_and_bounded_by_scheduler_rounds() {
        let run = || {
            let mut sim = SimNet::new(SimNetConfig {
                seed: 1234,
                jitter_rounds: 2,
                ..Default::default()
            });
            for frame in [b"first".as_slice(), b"second", b"third"] {
                sim.write_frame(frame);
            }
            for _ in 0..2 {
                sim.advance_round();
            }
            let mut delivered = Vec::new();
            let mut buffer = [0; 16];
            while let Some(length) = sim.read_frame(&mut buffer) {
                delivered.push(buffer[..length].to_vec());
            }
            assert_eq!(delivered.len(), 3, "jitter must not exceed two rounds");
            delivered
        };

        assert_eq!(run(), run(), "the same seed must choose the same jitter");
    }

    #[test]
    fn test_shared_switch_delivers_ready_frames_before_earlier_delayed_frames() {
        let mut switch = SimSwitch::new();
        switch.attach("api").unwrap();
        switch.attach("worker").unwrap();
        switch.deliver("api", false, 2, b"first");
        switch.deliver("api", false, 0, b"second");

        assert_eq!(switch.receive("worker"), Some(b"second".to_vec()));
        switch.advance_round();
        assert_eq!(switch.receive("worker"), None);
        switch.advance_round();
        assert_eq!(switch.receive("worker"), Some(b"first".to_vec()));
    }

    #[test]
    fn test_drops_are_deterministic() {
        let cfg = SimNetConfig {
            seed: 1234,
            loopback: true,
            drop_ppm: 500_000, // drop ~half
            partitioned: false,
            latency_rounds: 0,
            jitter_rounds: 0,
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

    #[test]
    fn test_shared_switch_delivers_to_other_named_ports_in_order() {
        let switch = Arc::new(Mutex::new(SimSwitch::new()));
        let mut api = SimNet::new_with_switch(
            SimNetConfig {
                loopback: false,
                ..Default::default()
            },
            switch.clone(),
            "backplane/api",
        )
        .unwrap();
        let mut worker = SimNet::new_with_switch(
            SimNetConfig {
                loopback: false,
                ..Default::default()
            },
            switch.clone(),
            "backplane/worker",
        )
        .unwrap();

        api.write_frame(b"first");
        api.write_frame(b"second");
        assert!(!api.has_pending_rx());
        assert!(worker.has_pending_rx());
        let mut buf = [0u8; 16];
        let first = worker.read_frame(&mut buf).unwrap();
        assert_eq!(&buf[..first], b"first");
        let second = worker.read_frame(&mut buf).unwrap();
        assert_eq!(&buf[..second], b"second");
        assert_eq!(
            switch.lock().unwrap().ports(),
            ["backplane/api", "backplane/worker"]
        );
    }

    #[test]
    fn test_shared_switch_rejects_duplicate_ports() {
        let switch = Arc::new(Mutex::new(SimSwitch::new()));
        let _api = SimNet::new_with_switch(Default::default(), switch.clone(), "api").unwrap();
        assert_eq!(
            SimNet::new_with_switch(Default::default(), switch, "api").unwrap_err(),
            SimSwitchError::DuplicatePort("api".to_owned())
        );
    }

    #[test]
    fn test_shared_switch_delay_advances_once_for_all_ports() {
        let switch = Arc::new(Mutex::new(SimSwitch::new()));
        let mut api = SimNet::new_with_switch(
            SimNetConfig {
                loopback: false,
                latency_rounds: 1,
                ..Default::default()
            },
            switch.clone(),
            "backplane/api",
        )
        .unwrap();
        let worker = SimNet::new_with_switch(
            SimNetConfig {
                loopback: false,
                ..Default::default()
            },
            switch.clone(),
            "backplane/worker",
        )
        .unwrap();

        api.write_frame(b"delayed");
        assert!(!worker.has_pending_rx());
        switch.lock().unwrap().advance_round();
        assert!(worker.has_pending_rx());
    }
}
