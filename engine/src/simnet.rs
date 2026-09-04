// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Simulated network backend — deterministic, host-independent packet I/O.
//!
//! Replaces the host tap device behind the virtio-net frontend. The guest sees
//! a normal NIC; frames never touch the host network. It provides deterministic
//! fault and link primitives:
//!
//! - **loopback**: TX frames are queued back for RX delivery (driver bring-up
//!   without any host networking)
//! - **partition**: all traffic is dropped in both directions
//! - **deterministic random drops**: per-frame drops driven by a seeded ChaCha
//!   stream, so a given seed + frame sequence always produces the same drops
//! - **deterministic duplication**: an accepted frame can be sent twice through
//!   the same simulated link
//! - **deterministic corruption**: a selected nonempty frame has one seeded bit
//!   flipped before link delivery
//! - **bounded transmit queues**: excess frames are dropped at a configured
//!   per-NIC queue limit
//! - **bounded receive queues**: frames are dropped when a guest stops
//!   consuming a configured per-NIC ingress queue
//!
//! Frame delay, jitter, and bandwidth all advance in deterministic runner
//! rounds, never host time.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// Duplication probability, parts per million, applied after a frame is
    /// accepted. A duplicate travels through the same simulated link. 0 =
    /// never duplicate.
    #[serde(default)]
    pub duplicate_ppm: u32,
    /// Corruption probability, parts per million, applied to accepted nonempty
    /// frames. A selected frame has one seeded bit flipped before link delivery.
    /// 0 = never corrupt.
    #[serde(default)]
    pub corrupt_ppm: u32,
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
    /// Outbound byte budget refilled once per deterministic scheduler round.
    /// Zero leaves the link unlimited.
    #[serde(default)]
    pub tx_bytes_per_round: u64,
    /// Maximum transmitted Ethernet frame size. Zero leaves the link unlimited.
    #[serde(default)]
    pub mtu_bytes: u32,
    /// Maximum number of frames waiting for outbound link budget. Zero leaves
    /// the transmit queue unlimited.
    #[serde(default)]
    pub tx_queue_frames: u32,
    /// Maximum number of frames waiting for guest RX delivery. Zero leaves the
    /// receive queue unlimited.
    #[serde(default)]
    pub rx_queue_frames: u32,
}

impl Default for SimNetConfig {
    fn default() -> Self {
        SimNetConfig {
            seed: 0,
            loopback: true,
            drop_ppm: 0,
            duplicate_ppm: 0,
            corrupt_ppm: 0,
            partitioned: false,
            latency_rounds: 0,
            jitter_rounds: 0,
            tx_bytes_per_round: 0,
            mtu_bytes: 0,
            tx_queue_frames: 0,
            rx_queue_frames: 0,
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
    /// Extra frames created by deterministic duplication.
    pub duplicated: u64,
    /// Frames changed by deterministic one-bit corruption.
    pub corrupted: u64,
    /// Digest of frames emitted to the simulated link, in submission order.
    pub tx_sha256: [u8; 32],
    /// Digest of frames successfully delivered to the guest RX path, in order.
    pub rx_sha256: [u8; 32],
}

/// Direction of a frame captured by the bounded simulated-link trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimNetFrameDirection {
    Tx,
    Rx,
    Drop,
}

/// Why a frame was deterministically discarded by the simulated NIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimNetDropReason {
    Mtu,
    Partition,
    LinkPartition,
    RandomLoss,
    TransmitQueue,
    ReceiveQueue,
    ReceiveBuffer,
}

/// One frame observed at a simulated NIC boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimNetFrame {
    pub round: u64,
    pub direction: SimNetFrameDirection,
    pub drop_reason: Option<SimNetDropReason>,
    pub bytes: Vec<u8>,
}

const FRAME_TRACE_LIMIT: usize = 64;

#[derive(Debug)]
struct PendingFrame {
    ready_round: u64,
    sequence: u64,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct PendingTransmit {
    delay_rounds: u32,
    bytes: Vec<u8>,
}

/// The simulated backend. Pure safe Rust; the only entropy source is the
/// seeded RNG.
#[derive(Debug)]
pub struct SimNet {
    config: SimNetConfig,
    /// Frames awaiting RX delivery to the guest.
    rx_queue: VecDeque<PendingFrame>,
    tx_queue: VecDeque<PendingTransmit>,
    round: u64,
    next_frame: u64,
    tx_bytes_remaining: u64,
    rng: ChaCha8Rng,
    /// Frames accepted from the guest TX path.
    pub tx_frames: u64,
    /// Frames delivered to the guest RX path.
    pub rx_frames: u64,
    /// Frames dropped by partition or the drop stream.
    pub dropped: u64,
    /// Extra frames queued by the deterministic duplication stream.
    pub duplicated: u64,
    /// Frames changed by deterministic one-bit corruption.
    pub corrupted: u64,
    tx_hasher: Sha256,
    rx_hasher: Sha256,
    trace: Arc<Mutex<Vec<SimNetFrame>>>,
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
#[derive(Debug)]
struct SimSwitchPort {
    rx_queue: VecDeque<PendingFrame>,
    rx_queue_frames: u32,
    dropped: u64,
    trace: Arc<Mutex<Vec<SimNetFrame>>>,
}

#[derive(Debug, Default)]
pub struct SimSwitch {
    ports: BTreeMap<String, SimSwitchPort>,
    /// Directed source-to-destination blackholes. A network partition can
    /// isolate every port; this set models the more common asymmetric outage
    /// where only one service-to-service path is unavailable.
    blocked_links: BTreeSet<(String, String)>,
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

    fn attach(
        &mut self,
        port: &str,
        rx_queue_frames: u32,
        trace: Arc<Mutex<Vec<SimNetFrame>>>,
    ) -> Result<(), SimSwitchError> {
        if port.is_empty() {
            return Err(SimSwitchError::InvalidPort(port.to_owned()));
        }
        if self.ports.contains_key(port) {
            return Err(SimSwitchError::DuplicatePort(port.to_owned()));
        }
        self.ports.insert(
            port.to_owned(),
            SimSwitchPort {
                rx_queue: VecDeque::new(),
                rx_queue_frames,
                dropped: 0,
                trace,
            },
        );
        Ok(())
    }

    fn detach(&mut self, port: &str) {
        self.ports.remove(port);
        self.blocked_links
            .retain(|(source, destination)| source != port && destination != port);
    }

    /// Block or restore a single directed path between two attached ports.
    /// Existing queued frames are left intact so the action only affects new
    /// traffic in the deterministic timeline.
    pub fn set_link_blocked(
        &mut self,
        source: &str,
        destination: &str,
        blocked: bool,
    ) -> Result<(), SimSwitchError> {
        if !self.ports.contains_key(source) {
            return Err(SimSwitchError::InvalidPort(source.to_owned()));
        }
        if !self.ports.contains_key(destination) {
            return Err(SimSwitchError::InvalidPort(destination.to_owned()));
        }
        let link = (source.to_owned(), destination.to_owned());
        if blocked {
            self.blocked_links.insert(link);
        } else {
            self.blocked_links.remove(&link);
        }
        Ok(())
    }

    /// Change a destination port's bound for subsequently delivered frames.
    /// Existing queued frames remain intact so this is a timeline action, not
    /// a retroactive mutation of already-observed traffic.
    pub fn set_rx_queue_frames(&mut self, port: &str, rx_queue_frames: u32) -> bool {
        let Some(destination) = self.ports.get_mut(port) else {
            return false;
        };
        destination.rx_queue_frames = rx_queue_frames;
        true
    }

    fn deliver(&mut self, source: &str, include_source: bool, delay_rounds: u32, frame: &[u8]) {
        let ready_round = self.round.saturating_add(u64::from(delay_rounds));
        let sequence = self.next_frame;
        self.next_frame = self.next_frame.saturating_add(1);
        for (port, destination) in &mut self.ports {
            if include_source || port != source {
                if self
                    .blocked_links
                    .contains(&(source.to_owned(), port.clone()))
                {
                    destination.dropped += 1;
                    trace_drop(
                        &destination.trace,
                        self.round,
                        SimNetDropReason::LinkPartition,
                        frame,
                    );
                    continue;
                }
                if destination.rx_queue_frames != 0
                    && destination.rx_queue.len() >= destination.rx_queue_frames as usize
                {
                    destination.dropped += 1;
                    trace_drop(
                        &destination.trace,
                        self.round,
                        SimNetDropReason::ReceiveQueue,
                        frame,
                    );
                    continue;
                }
                push_pending(
                    &mut destination.rx_queue,
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
        let queue = &mut self.ports.get_mut(port)?.rx_queue;
        (queue.front()?.ready_round <= self.round).then(|| queue.pop_front().unwrap().bytes)
    }

    fn has_pending_rx(&self, port: &str) -> bool {
        self.ports
            .get(port)
            .and_then(|destination| destination.rx_queue.front())
            .is_some_and(|frame| frame.ready_round <= self.round)
    }

    fn dropped(&self, port: &str) -> u64 {
        self.ports
            .get(port)
            .map_or(0, |destination| destination.dropped)
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

/// Hash a length-delimited frame so different frame boundaries cannot produce
/// the same byte stream fingerprint.
fn update_frame_digest(hasher: &mut Sha256, frame: &[u8]) {
    hasher.update(
        u64::try_from(frame.len())
            .expect("frame length fits in u64")
            .to_le_bytes(),
    );
    hasher.update(frame);
}

fn trace_frame(
    trace: &Arc<Mutex<Vec<SimNetFrame>>>,
    round: u64,
    direction: SimNetFrameDirection,
    drop_reason: Option<SimNetDropReason>,
    bytes: &[u8],
) {
    let mut trace = trace.lock().expect("simulated trace lock poisoned");
    if trace.len() < FRAME_TRACE_LIMIT {
        trace.push(SimNetFrame {
            round,
            direction,
            drop_reason,
            bytes: bytes.to_vec(),
        });
    }
}

fn trace_drop(
    trace: &Arc<Mutex<Vec<SimNetFrame>>>,
    round: u64,
    reason: SimNetDropReason,
    bytes: &[u8],
) {
    trace_frame(
        trace,
        round,
        SimNetFrameDirection::Drop,
        Some(reason),
        bytes,
    );
}

impl SimNet {
    pub fn new(config: SimNetConfig) -> Self {
        SimNet {
            config,
            rx_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
            round: 0,
            next_frame: 0,
            tx_bytes_remaining: config.tx_bytes_per_round,
            rng: ChaCha8Rng::seed_from_u64(config.seed),
            tx_frames: 0,
            rx_frames: 0,
            dropped: 0,
            duplicated: 0,
            corrupted: 0,
            tx_hasher: Sha256::new(),
            rx_hasher: Sha256::new(),
            trace: Arc::new(Mutex::new(Vec::new())),
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
        let trace = Arc::new(Mutex::new(Vec::new()));
        switch
            .lock()
            .expect("simulated switch lock poisoned")
            .attach(&endpoint, config.rx_queue_frames, trace.clone())?;
        Ok(SimNet {
            config,
            rx_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
            round: 0,
            next_frame: 0,
            tx_bytes_remaining: config.tx_bytes_per_round,
            rng: ChaCha8Rng::seed_from_u64(config.seed),
            tx_frames: 0,
            rx_frames: 0,
            dropped: 0,
            duplicated: 0,
            corrupted: 0,
            tx_hasher: Sha256::new(),
            rx_hasher: Sha256::new(),
            trace,
            switch: Some(switch),
            endpoint: Some(endpoint),
        })
    }

    pub fn config(&self) -> SimNetConfig {
        self.config
    }

    /// Change whether this simulated link drops newly transmitted frames as a
    /// partition. Queued frames are deliberately left alone: a topology
    /// action changes the link from this point in the deterministic timeline,
    /// without rewinding its seeded RNG or switch state.
    pub fn set_partitioned(&mut self, partitioned: bool) {
        self.config.partitioned = partitioned;
    }

    /// Replace the mutable packet-condition settings without disturbing this
    /// NIC's seeded RNG, counters, queues, partition state, or topology-switch
    /// attachment. A campaign can therefore change conditions at a UART
    /// barrier and later restore its declared baseline on the same timeline.
    pub fn set_conditions(&mut self, conditions: SimNetConfig) {
        self.config.drop_ppm = conditions.drop_ppm;
        self.config.duplicate_ppm = conditions.duplicate_ppm;
        self.config.corrupt_ppm = conditions.corrupt_ppm;
        self.config.latency_rounds = conditions.latency_rounds;
        self.config.jitter_rounds = conditions.jitter_rounds;
        self.config.tx_bytes_per_round = conditions.tx_bytes_per_round;
        self.config.mtu_bytes = conditions.mtu_bytes;
        self.config.tx_queue_frames = conditions.tx_queue_frames;
        self.config.rx_queue_frames = conditions.rx_queue_frames;
        if let (Some(switch), Some(endpoint)) = (&self.switch, &self.endpoint) {
            let updated = switch
                .lock()
                .expect("simulated switch lock poisoned")
                .set_rx_queue_frames(endpoint, self.config.rx_queue_frames);
            debug_assert!(updated, "attached simulated NIC lost its switch port");
        }
        if self.config.tx_bytes_per_round != 0 {
            self.tx_bytes_remaining = self.config.tx_bytes_per_round;
        }
    }

    /// Block or restore this NIC's directed path to one peer on its shared
    /// deterministic switch. Returns false for standalone loopback NICs.
    pub fn set_link_blocked(&mut self, destination: &str, blocked: bool) -> bool {
        let (Some(switch), Some(source)) = (&self.switch, &self.endpoint) else {
            return false;
        };
        switch
            .lock()
            .expect("simulated switch lock poisoned")
            .set_link_blocked(source, destination, blocked)
            .is_ok()
    }

    /// Return the stable switch port name for a topology NIC.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Return deterministic frame counters for this NIC.
    pub fn stats(&self) -> SimNetStats {
        let ingress_dropped = if let (Some(switch), Some(endpoint)) = (&self.switch, &self.endpoint)
        {
            switch
                .lock()
                .expect("simulated switch lock poisoned")
                .dropped(endpoint)
        } else {
            0
        };
        SimNetStats {
            tx_frames: self.tx_frames,
            rx_frames: self.rx_frames,
            dropped: self.dropped.saturating_add(ingress_dropped),
            duplicated: self.duplicated,
            corrupted: self.corrupted,
            tx_sha256: self.tx_hasher.clone().finalize().into(),
            rx_sha256: self.rx_hasher.clone().finalize().into(),
        }
    }

    /// Return the first 64 transmitted, dropped, and delivered frames in
    /// deterministic boundary order. The fixed bound prevents a guest from
    /// growing bundles without limit.
    pub fn trace(&self) -> Vec<SimNetFrame> {
        self.trace
            .lock()
            .expect("simulated trace lock poisoned")
            .clone()
    }

    fn trace_frame(&mut self, direction: SimNetFrameDirection, bytes: &[u8]) {
        trace_frame(&self.trace, self.round, direction, None, bytes);
    }

    fn trace_drop(&mut self, reason: SimNetDropReason, bytes: &[u8]) {
        trace_drop(&self.trace, self.round, reason, bytes);
    }

    /// Advance this simulated NIC by one deterministic runner round.
    ///
    /// Topology-owned NICs share a switch, which its runner advances exactly
    /// once after every service has been pumped. The source link's byte budget
    /// still refills once per round for every simulated NIC.
    pub fn advance_round(&mut self) {
        self.round = self.round.saturating_add(1);
        if self.config.tx_bytes_per_round != 0 {
            self.tx_bytes_remaining = self.config.tx_bytes_per_round;
        }
        self.drain_transmit_queue();
    }

    /// Deterministic per-frame drop decision.
    fn drop_reason(&mut self) -> Option<SimNetDropReason> {
        if self.config.partitioned {
            return Some(SimNetDropReason::Partition);
        }
        if self.config.drop_ppm == 0 {
            return None;
        }
        (self.rng.next_u32() % 1_000_000 < self.config.drop_ppm)
            .then_some(SimNetDropReason::RandomLoss)
    }

    /// Deterministic per-frame duplication decision, evaluated only after a
    /// frame survived dropping.
    fn should_duplicate(&mut self) -> bool {
        if self.config.duplicate_ppm == 0 {
            return false;
        }
        self.rng.next_u32() % 1_000_000 < self.config.duplicate_ppm
    }

    /// Deterministic per-frame corruption decision, evaluated after dropping.
    fn should_corrupt(&mut self) -> bool {
        if self.config.corrupt_ppm == 0 {
            return false;
        }
        self.rng.next_u32() % 1_000_000 < self.config.corrupt_ppm
    }

    /// Flip one bit selected from the same seeded stream as other link faults.
    /// The caller excludes empty frames, so the modulus is always defined.
    fn corrupt_frame(&mut self, frame: &mut [u8]) {
        let frame_len = u64::try_from(frame.len()).expect("frame length fits in u64");
        let byte_index =
            usize::try_from(self.rng.next_u64() % frame_len).expect("frame index fits in usize");
        let bit = self.rng.next_u32() % 8;
        frame[byte_index] ^= 1_u8 << bit;
    }

    /// The base delay plus a seeded, per-frame jitter. Use a u64 modulus so
    /// the full u32 configuration range remains valid.
    fn delivery_delay_rounds(&mut self) -> u32 {
        let jitter =
            u32::try_from(self.rng.next_u64() % (u64::from(self.config.jitter_rounds) + 1))
                .expect("configured jitter is bounded by u32");
        self.config.latency_rounds.saturating_add(jitter)
    }

    fn can_transmit(&self, frame_len: usize) -> bool {
        if self.config.tx_bytes_per_round == 0 {
            return true;
        }
        let frame_len = u64::try_from(frame_len).expect("frame length fits in u64");
        self.tx_bytes_remaining >= frame_len
            || self.tx_bytes_remaining == self.config.tx_bytes_per_round
    }

    fn spend_transmit_budget(&mut self, frame_len: usize) {
        if self.config.tx_bytes_per_round != 0 {
            let frame_len = u64::try_from(frame_len).expect("frame length fits in u64");
            self.tx_bytes_remaining = self.tx_bytes_remaining.saturating_sub(frame_len);
        }
    }

    fn deliver_transmitted_frame(&mut self, frame: PendingTransmit) {
        update_frame_digest(&mut self.tx_hasher, &frame.bytes);
        self.trace_frame(SimNetFrameDirection::Tx, &frame.bytes);
        if let (Some(switch), Some(endpoint)) = (&self.switch, &self.endpoint) {
            switch
                .lock()
                .expect("simulated switch lock poisoned")
                .deliver(
                    endpoint,
                    self.config.loopback,
                    frame.delay_rounds,
                    &frame.bytes,
                );
        } else if self.config.loopback {
            let frame = PendingFrame {
                ready_round: self.round.saturating_add(u64::from(frame.delay_rounds)),
                sequence: self.next_frame,
                bytes: frame.bytes,
            };
            self.next_frame = self.next_frame.saturating_add(1);
            self.queue_receive(frame);
        }
    }

    fn drain_transmit_queue(&mut self) {
        while self
            .tx_queue
            .front()
            .is_some_and(|frame| self.can_transmit(frame.bytes.len()))
        {
            let frame = self.tx_queue.pop_front().expect("checked queue front");
            self.spend_transmit_budget(frame.bytes.len());
            self.deliver_transmitted_frame(frame);
        }
    }

    /// Queue a frame for the simulated link, dropping it deterministically if
    /// the configured outbound queue is full.
    fn queue_transmit(&mut self, frame: PendingTransmit) -> bool {
        if self.config.tx_queue_frames != 0
            && self.tx_queue.len() >= self.config.tx_queue_frames as usize
        {
            self.dropped += 1;
            self.trace_drop(SimNetDropReason::TransmitQueue, &frame.bytes);
            return false;
        }
        self.tx_queue.push_back(frame);
        true
    }

    /// Queue a frame for guest RX delivery, dropping it deterministically if
    /// the configured ingress queue is full.
    fn queue_receive(&mut self, frame: PendingFrame) {
        if self.config.rx_queue_frames != 0
            && self.rx_queue.len() >= self.config.rx_queue_frames as usize
        {
            self.dropped += 1;
            self.trace_drop(SimNetDropReason::ReceiveQueue, &frame.bytes);
            return;
        }
        push_pending(&mut self.rx_queue, frame);
    }

    /// Accept a frame from the guest TX path.
    pub fn write_frame(&mut self, frame: &[u8]) {
        self.tx_frames += 1;
        if self.config.mtu_bytes != 0 && frame.len() > self.config.mtu_bytes as usize {
            self.dropped += 1;
            self.trace_drop(SimNetDropReason::Mtu, frame);
            return;
        }
        if let Some(reason) = self.drop_reason() {
            self.dropped += 1;
            self.trace_drop(reason, frame);
            return;
        }
        let delay_rounds = self.delivery_delay_rounds();
        let mut bytes = frame.to_vec();
        if !bytes.is_empty() && self.should_corrupt() {
            self.corrupt_frame(&mut bytes);
            self.corrupted += 1;
        }
        self.queue_transmit(PendingTransmit {
            delay_rounds,
            bytes: bytes.clone(),
        });
        if self.should_duplicate() {
            if self.queue_transmit(PendingTransmit {
                delay_rounds,
                bytes,
            }) {
                self.duplicated += 1;
            }
        }
        self.drain_transmit_queue();
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
            self.trace_drop(SimNetDropReason::ReceiveBuffer, &frame);
            return None;
        }
        buf[..frame.len()].copy_from_slice(&frame);
        self.rx_frames += 1;
        update_frame_digest(&mut self.rx_hasher, &frame);
        self.trace_frame(SimNetFrameDirection::Rx, &frame);
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
        let stats = sim.stats();
        assert_eq!(stats.tx_frames, 1);
        assert_eq!(stats.rx_frames, 1);
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.duplicated, 0);
        assert_eq!(stats.corrupted, 0);
        assert_eq!(stats.tx_sha256, stats.rx_sha256);
        assert_ne!(stats.tx_sha256, [0; 32]);
    }

    #[test]
    fn partition_state_changes_without_resetting_link_statistics() {
        let mut sim = SimNet::new(SimNetConfig {
            loopback: true,
            ..Default::default()
        });
        sim.write_frame(&[1]);
        assert_eq!(sim.stats().tx_frames, 1);
        sim.set_partitioned(true);
        sim.write_frame(&[2]);
        assert!(sim.config().partitioned);
        assert_eq!(sim.stats().tx_frames, 2);
        assert_eq!(sim.stats().dropped, 1);
        sim.set_partitioned(false);
        sim.write_frame(&[3]);
        assert!(!sim.config().partitioned);
        assert_eq!(sim.stats().tx_frames, 3);
    }

    #[test]
    fn conditions_change_without_resetting_partition_or_statistics() {
        let mut sim = SimNet::new(SimNetConfig {
            loopback: true,
            seed: 42,
            partitioned: true,
            ..Default::default()
        });
        sim.write_frame(b"before");
        sim.set_conditions(SimNetConfig {
            drop_ppm: 100,
            duplicate_ppm: 200,
            corrupt_ppm: 300,
            latency_rounds: 4,
            jitter_rounds: 5,
            tx_bytes_per_round: 6,
            mtu_bytes: 7,
            tx_queue_frames: 8,
            rx_queue_frames: 9,
            ..Default::default()
        });
        let config = sim.config();
        assert_eq!(config.seed, 42);
        assert!(config.partitioned);
        assert_eq!(config.drop_ppm, 100);
        assert_eq!(config.duplicate_ppm, 200);
        assert_eq!(config.corrupt_ppm, 300);
        assert_eq!(config.latency_rounds, 4);
        assert_eq!(config.jitter_rounds, 5);
        assert_eq!(config.tx_bytes_per_round, 6);
        assert_eq!(config.mtu_bytes, 7);
        assert_eq!(config.tx_queue_frames, 8);
        assert_eq!(config.rx_queue_frames, 9);
        assert_eq!(sim.stats().tx_frames, 1);
    }

    #[test]
    fn test_frame_digests_are_deterministic_and_ordered() {
        let run = |frames: &[&[u8]]| {
            let mut sim = SimNet::new(SimNetConfig::default());
            let mut buffer = [0; 16];
            for frame in frames {
                sim.write_frame(frame);
                sim.read_frame(&mut buffer).unwrap();
            }
            sim.stats()
        };

        let first = run(&[b"one", b"two"]);
        let same = run(&[b"one", b"two"]);
        let reversed = run(&[b"two", b"one"]);
        assert_eq!(first.tx_sha256, same.tx_sha256);
        assert_eq!(first.rx_sha256, same.rx_sha256);
        assert_ne!(first.tx_sha256, reversed.tx_sha256);
        assert_ne!(first.rx_sha256, reversed.rx_sha256);
    }

    #[test]
    fn test_frame_trace_records_boundaries_with_a_fixed_limit() {
        let mut sim = SimNet::new(SimNetConfig::default());
        sim.write_frame(b"trace");
        let mut buffer = [0; 16];
        sim.read_frame(&mut buffer).unwrap();
        assert_eq!(sim.trace().len(), 2);
        assert_eq!(sim.trace()[0].direction, SimNetFrameDirection::Tx);
        assert_eq!(sim.trace()[1].direction, SimNetFrameDirection::Rx);
        assert_eq!(sim.trace()[0].drop_reason, None);
        assert_eq!(sim.trace()[0].bytes, b"trace");

        for _ in 0..64 {
            sim.write_frame(b"x");
            sim.read_frame(&mut buffer).unwrap();
        }
        assert_eq!(sim.trace().len(), FRAME_TRACE_LIMIT);
    }

    #[test]
    fn test_frame_trace_records_deterministic_drop_reasons() {
        let mut sim = SimNet::new(SimNetConfig {
            mtu_bytes: 3,
            ..Default::default()
        });
        sim.write_frame(b"oversized");

        assert_eq!(sim.trace().len(), 1);
        assert_eq!(sim.trace()[0].direction, SimNetFrameDirection::Drop);
        assert_eq!(sim.trace()[0].drop_reason, Some(SimNetDropReason::Mtu));
        assert_eq!(sim.trace()[0].bytes, b"oversized");
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
    fn test_mtu_drops_only_oversized_frames() {
        let mut sim = SimNet::new(SimNetConfig {
            mtu_bytes: 3,
            ..Default::default()
        });
        sim.write_frame(b"fit");
        sim.write_frame(b"oversized");
        let mut buffer = [0; 16];
        assert_eq!(sim.read_frame(&mut buffer), Some(3));
        assert_eq!(sim.dropped, 1);
    }

    #[test]
    fn test_transmit_queue_drops_frames_after_deterministic_overflow() {
        let mut sim = SimNet::new(SimNetConfig {
            tx_bytes_per_round: 3,
            tx_queue_frames: 1,
            ..Default::default()
        });
        sim.write_frame(b"one");
        sim.write_frame(b"two");
        sim.write_frame(b"three");

        let mut buffer = [0; 16];
        assert_eq!(sim.read_frame(&mut buffer), Some(3));
        assert_eq!(&buffer[..3], b"one");
        assert_eq!(sim.stats().dropped, 1);
        assert_eq!(
            sim.trace()[1].drop_reason,
            Some(SimNetDropReason::TransmitQueue)
        );

        sim.advance_round();
        assert_eq!(sim.read_frame(&mut buffer), Some(3));
        assert_eq!(&buffer[..3], b"two");
        assert!(!sim.has_pending_rx());
    }

    #[test]
    fn test_receive_queue_drops_frames_after_deterministic_overflow() {
        let mut sim = SimNet::new(SimNetConfig {
            rx_queue_frames: 1,
            ..Default::default()
        });
        sim.write_frame(b"first");
        sim.write_frame(b"second");

        assert_eq!(sim.stats().dropped, 1);
        assert_eq!(
            sim.trace()[2].drop_reason,
            Some(SimNetDropReason::ReceiveQueue)
        );
        let mut buffer = [0; 16];
        assert_eq!(sim.read_frame(&mut buffer), Some(5));
        assert_eq!(&buffer[..5], b"first");
        assert!(!sim.has_pending_rx());
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
    fn test_bandwidth_releases_frames_in_deterministic_rounds() {
        let mut sim = SimNet::new(SimNetConfig {
            tx_bytes_per_round: 3,
            ..Default::default()
        });
        sim.write_frame(b"one");
        sim.write_frame(b"two");

        let mut buffer = [0; 16];
        let length = sim.read_frame(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"one");
        assert!(!sim.has_pending_rx());

        sim.advance_round();
        let length = sim.read_frame(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"two");
    }

    #[test]
    fn test_bandwidth_allows_one_oversized_frame_per_round() {
        let mut sim = SimNet::new(SimNetConfig {
            tx_bytes_per_round: 3,
            ..Default::default()
        });
        sim.write_frame(b"large");
        sim.write_frame(b"again");

        let mut buffer = [0; 16];
        let length = sim.read_frame(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"large");
        assert!(!sim.has_pending_rx());
        sim.advance_round();
        let length = sim.read_frame(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"again");
    }

    #[test]
    fn test_duplication_uses_the_simulated_link() {
        let mut sim = SimNet::new(SimNetConfig {
            duplicate_ppm: 1_000_000,
            tx_bytes_per_round: 3,
            ..Default::default()
        });
        sim.write_frame(b"one");

        let mut buffer = [0; 16];
        let length = sim.read_frame(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"one");
        assert!(!sim.has_pending_rx(), "the duplicate waits for link budget");
        sim.advance_round();
        let length = sim.read_frame(&mut buffer).unwrap();
        assert_eq!(&buffer[..length], b"one");
        assert_eq!(sim.stats().duplicated, 1);
    }

    #[test]
    fn test_corruption_flips_one_seeded_bit_before_link_duplication() {
        let run = || {
            let mut sim = SimNet::new(SimNetConfig {
                seed: 1234,
                corrupt_ppm: 1_000_000,
                duplicate_ppm: 1_000_000,
                ..Default::default()
            });
            sim.write_frame(b"unchanged");
            let mut buffer = [0; 16];
            let first = sim.read_frame(&mut buffer).unwrap();
            let first = buffer[..first].to_vec();
            let second = sim.read_frame(&mut buffer).unwrap();
            (first, buffer[..second].to_vec(), sim.stats())
        };

        let (first, duplicate, stats) = run();
        assert_eq!(first, duplicate, "duplicates retain the corrupted bytes");
        assert_eq!(
            first
                .iter()
                .zip(b"unchanged")
                .map(|(actual, original)| (actual ^ original).count_ones())
                .sum::<u32>(),
            1,
            "one selected bit must change"
        );
        assert_eq!(stats.corrupted, 1);
        assert_eq!(stats.duplicated, 1);
        assert_eq!(run(), (first, duplicate, stats));
    }

    #[test]
    fn test_shared_switch_delivers_ready_frames_before_earlier_delayed_frames() {
        let mut switch = SimSwitch::new();
        switch
            .attach("api", 0, Arc::new(Mutex::new(Vec::new())))
            .unwrap();
        switch
            .attach("worker", 0, Arc::new(Mutex::new(Vec::new())))
            .unwrap();
        switch.deliver("api", false, 2, b"first");
        switch.deliver("api", false, 0, b"second");

        assert_eq!(switch.receive("worker"), Some(b"second".to_vec()));
        switch.advance_round();
        assert_eq!(switch.receive("worker"), None);
        switch.advance_round();
        assert_eq!(switch.receive("worker"), Some(b"first".to_vec()));
    }

    #[test]
    fn test_shared_switch_blocks_only_the_selected_directed_link() {
        let api_trace = Arc::new(Mutex::new(Vec::new()));
        let replica_trace = Arc::new(Mutex::new(Vec::new()));
        let auditor_trace = Arc::new(Mutex::new(Vec::new()));
        let mut switch = SimSwitch::new();
        switch.attach("api", 0, api_trace).unwrap();
        switch.attach("replica", 0, replica_trace.clone()).unwrap();
        switch.attach("auditor", 0, auditor_trace).unwrap();

        switch.set_link_blocked("api", "replica", true).unwrap();
        switch.deliver("api", false, 0, b"request");
        assert_eq!(switch.receive("replica"), None);
        assert_eq!(switch.receive("auditor"), Some(b"request".to_vec()));
        assert_eq!(switch.dropped("replica"), 1);
        assert!(replica_trace.lock().unwrap().iter().any(|frame| {
            frame.drop_reason == Some(SimNetDropReason::LinkPartition)
        }));

        switch.set_link_blocked("api", "replica", false).unwrap();
        switch.deliver("api", false, 0, b"retry");
        assert_eq!(switch.receive("replica"), Some(b"retry".to_vec()));
    }

    #[test]
    fn test_drops_are_deterministic() {
        let cfg = SimNetConfig {
            seed: 1234,
            loopback: true,
            drop_ppm: 500_000, // drop ~half
            duplicate_ppm: 0,
            corrupt_ppm: 0,
            partitioned: false,
            latency_rounds: 0,
            jitter_rounds: 0,
            tx_bytes_per_round: 0,
            mtu_bytes: 0,
            tx_queue_frames: 0,
            rx_queue_frames: 0,
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
    fn test_shared_switch_receive_queue_drops_at_the_destination() {
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
                rx_queue_frames: 1,
                ..Default::default()
            },
            switch,
            "backplane/worker",
        )
        .unwrap();

        api.write_frame(b"first");
        api.write_frame(b"second");

        assert_eq!(worker.stats().dropped, 1);
        assert_eq!(
            worker.trace()[0].drop_reason,
            Some(SimNetDropReason::ReceiveQueue)
        );
        let mut buffer = [0; 16];
        assert_eq!(worker.read_frame(&mut buffer), Some(5));
        assert_eq!(&buffer[..5], b"first");
    }

    #[test]
    fn shared_switch_conditions_update_the_live_receive_queue_limit() {
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
            switch,
            "backplane/worker",
        )
        .unwrap();

        worker.set_conditions(SimNetConfig {
            rx_queue_frames: 1,
            ..Default::default()
        });
        api.write_frame(b"first");
        api.write_frame(b"second");

        assert_eq!(worker.stats().dropped, 1);
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
