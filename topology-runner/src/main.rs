// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Linux+KVM executor for a normalized Theseus Compose topology plan.
//!
//! This stays separate from the portable `theseus` CLI: the CLI plans on
//! macOS, while this binary links Firecracker's Linux/KVM VMM and runs only
//! from a published Linux runtime bundle.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use theseus_engine::simnet::{SharedSimSwitch, SimSwitch};
use vmm::builder::build_microvm_for_boot;
use vmm::devices::virtio::block::device::Block;
use vmm::devices::virtio::block::virtio::device::SimulatedBlockConfig;
use vmm::devices::virtio::net::{Net, SimNetConfig, SimNetDropReason, SimNetFrameDirection};
use vmm::rate_limiter::RateLimiter;
use vmm::resources::VmResources;
use vmm::seccomp::get_empty_filters;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::entropy::EntropyDeviceConfig;
use vmm::vmm_config::instance_info::InstanceInfo;
use vmm::vmm_config::machine_config::{MachineConfigUpdate, VirtualTimeConfig};
use vmm::{EventManager, FcExitCode, Vmm};

const USAGE: &str =
    "Usage: theseus-topology --plan topology-plan.json --output replay-dir [--minimize]";

#[derive(Debug, Deserialize, Serialize)]
struct TopologyPlan {
    format: String,
    compose: String,
    services: BTreeMap<String, ServicePlan>,
    networks: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    campaign: Option<CampaignPlan>,
    #[serde(default)]
    topology_runner: Option<Artifact>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CampaignPlan {
    driver: String,
    operations: Vec<CampaignOperation>,
    #[serde(default)]
    faults: Vec<CampaignFault>,
    #[serde(default)]
    properties: Vec<CampaignProperty>,
    max_runs: u16,
    #[serde(default = "default_campaign_faults_per_run")]
    max_faults_per_run: u8,
}

fn default_campaign_faults_per_run() -> u8 {
    2
}

#[derive(Debug, Deserialize, Serialize)]
struct CampaignOperation {
    name: String,
    input_hex: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct CampaignFault {
    kind: CampaignFaultKind,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    drive: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    at_round: Option<u64>,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<u64>,
    #[serde(default)]
    error_ppm: Option<u32>,
    #[serde(default)]
    latency_rounds: Option<u32>,
    #[serde(default)]
    torn_write_bytes: Option<u32>,
    #[serde(default)]
    corrupt_read_xor: Option<u8>,
    #[serde(default)]
    drop_ppm: Option<u32>,
    #[serde(default)]
    duplicate_ppm: Option<u32>,
    #[serde(default)]
    corrupt_ppm: Option<u32>,
    #[serde(default)]
    jitter_rounds: Option<u32>,
    #[serde(default)]
    tx_bytes_per_round: Option<u64>,
    #[serde(default)]
    mtu_bytes: Option<u32>,
    #[serde(default)]
    tx_queue_frames: Option<u32>,
    #[serde(default)]
    rx_queue_frames: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignFaultKind {
    Pause,
    Restart,
    ClockJump,
    Partition,
    Heal,
    LinkPartition,
    LinkHeal,
    StorageFault,
    StorageRecover,
    NetworkFault,
    NetworkRecover,
}

#[derive(Debug, Deserialize, Serialize)]
struct CampaignProperty {
    name: String,
    kind: PropertyKind,
    contains: String,
    #[serde(default)]
    service: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PropertyKind {
    Always,
    Sometimes,
    Reachable,
    Unreachable,
}

#[derive(Debug, Serialize)]
struct CampaignResult {
    format: &'static str,
    status: &'static str,
    driver: String,
    runs: Vec<CampaignRun>,
    properties: Vec<CampaignPropertyResult>,
}

#[derive(Debug, Serialize)]
struct CampaignRun {
    index: usize,
    operations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fault: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    faults: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    actions: Vec<AppliedCampaignAction>,
    status: &'static str,
    novelty: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CampaignPropertyResult {
    name: String,
    kind: &'static str,
    status: &'static str,
    detail: String,
}

#[derive(Debug, Deserialize)]
struct RecordedCampaignResult {
    runs: Vec<RecordedCampaignRun>,
}

#[derive(Debug, Deserialize)]
struct RecordedCampaignRun {
    operations: Vec<String>,
    #[serde(default)]
    fault: Option<String>,
    #[serde(default)]
    faults: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CampaignMinimization {
    property: String,
    original_operations: Vec<String>,
    minimized_operations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fault: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    faults: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ServicePlan {
    manifest: String,
    run: RunPlan,
    networks: Vec<String>,
    #[serde(default)]
    faults: Vec<FaultPlan>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FaultPlan {
    at_round: u64,
    kind: FaultKind,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FaultKind {
    Pause,
    Restart,
    ClockJump,
}

#[derive(Debug, Deserialize, Serialize)]
struct RunPlan {
    format: String,
    manifest: String,
    runtime: RuntimePlan,
    guest: GuestPlan,
    run: RunConfig,
    #[serde(default)]
    network: NetworkConfig,
    #[serde(default)]
    storage: Vec<StoragePlan>,
    #[serde(default)]
    events: Vec<EventPlan>,
    #[serde(default)]
    checks: Vec<CheckPlan>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RuntimePlan {
    firecracker: Artifact,
}
#[derive(Debug, Deserialize, Serialize)]
struct GuestPlan {
    kernel: Artifact,
    initramfs: Artifact,
}
#[derive(Debug, Deserialize, Serialize)]
struct Artifact {
    path: String,
    sha256: String,
}
#[derive(Debug, Deserialize, Serialize)]
struct RunConfig {
    seed: u64,
    vcpu_count: u8,
    mem_size_mib: u32,
    timeout_secs: u64,
    virtual_time: Option<VirtualTime>,
}
#[derive(Debug, Deserialize, Serialize)]
struct VirtualTime {
    tick_ns: u64,
    exits_per_tick: u32,
}
#[derive(Debug, Default, Deserialize, Serialize)]
struct NetworkConfig {
    loopback: bool,
    drop_ppm: u32,
    partitioned: bool,
    #[serde(default)]
    latency_rounds: u32,
    #[serde(default)]
    jitter_rounds: u32,
    #[serde(default)]
    duplicate_ppm: u32,
    #[serde(default)]
    corrupt_ppm: u32,
    #[serde(default)]
    tx_bytes_per_round: u64,
    #[serde(default)]
    mtu_bytes: u32,
    #[serde(default)]
    tx_queue_frames: u32,
    #[serde(default)]
    rx_queue_frames: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct StoragePlan {
    id: String,
    size_mib: u32,
    seed: u64,
    error_ppm: u32,
    latency_rounds: u32,
    torn_write_bytes: Option<u32>,
    corrupt_read_xor: Option<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct EventPlan {
    data_hex: String,
    /// Campaign operations use an explicit serial barrier.  Ordinary manifest
    /// events leave it absent and retain the original fire-and-forget mode.
    #[serde(default)]
    checkpoint: Option<String>,
    /// Topology mutations deliberately occur only after the event's serial
    /// checkpoint, so the next operation observes the new state.
    #[serde(default)]
    actions: Vec<CampaignAction>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CampaignAction {
    operation: String,
    kind: CampaignFaultKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    torn_write_bytes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    corrupt_read_xor: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drop_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duplicate_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    corrupt_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jitter_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_bytes_per_round: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mtu_bytes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_queue_frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rx_queue_frames: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CheckPlan {
    name: String,
    kind: CheckKind,
    value: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckKind {
    SerialContains,
    SerialNotContains,
    MarkerSeen,
    MarkerNotSeen,
}

#[derive(Debug, Serialize)]
struct CheckResult {
    name: String,
    status: &'static str,
    detail: String,
}

#[derive(Debug, Serialize)]
struct ServiceResult {
    status: &'static str,
    serial_log: String,
    serial_logs: Vec<String>,
    serial_sha256: Vec<String>,
    faults_sha256: String,
    storage_sha256: BTreeMap<String, String>,
    network_traffic: BTreeMap<String, NetworkTraffic>,
    network_trace: BTreeMap<String, Vec<NetworkFrame>>,
    virtual_time_ns: Option<Vec<u64>>,
    error: Option<String>,
    checks: Vec<CheckResult>,
    faults: Vec<AppliedFault>,
}

#[derive(Debug, Deserialize)]
struct RecordedServiceResult {
    #[serde(default)]
    serial_log: Option<String>,
    #[serde(default)]
    serial_logs: Vec<String>,
    #[serde(default)]
    serial_sha256: Vec<String>,
    #[serde(default)]
    faults_sha256: Option<String>,
    #[serde(default)]
    faults: Vec<AppliedFault>,
    #[serde(default)]
    storage_sha256: Option<BTreeMap<String, String>>,
    #[serde(default)]
    network_traffic: Option<BTreeMap<String, NetworkTraffic>>,
    #[serde(default)]
    virtual_time_ns: Option<Option<Vec<u64>>>,
}

/// Deterministic simulated-NIC counters for one service network.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct NetworkTraffic {
    tx_frames: u64,
    rx_frames: u64,
    dropped: u64,
    #[serde(default)]
    duplicated: u64,
    #[serde(default)]
    corrupted: u64,
    #[serde(default)]
    tx_sha256: Option<String>,
    #[serde(default)]
    rx_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct NetworkFrame {
    round: u64,
    direction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    drop_reason: Option<String>,
    data_hex: String,
}

impl NetworkTraffic {
    fn add(&mut self, other: &Self) {
        self.tx_frames = self.tx_frames.saturating_add(other.tx_frames);
        self.rx_frames = self.rx_frames.saturating_add(other.rx_frames);
        self.dropped = self.dropped.saturating_add(other.dropped);
        self.duplicated = self.duplicated.saturating_add(other.duplicated);
        self.corrupted = self.corrupted.saturating_add(other.corrupted);
        self.tx_sha256 = combine_frame_digests(self.tx_sha256.take(), other.tx_sha256.as_deref());
        self.rx_sha256 = combine_frame_digests(self.rx_sha256.take(), other.rx_sha256.as_deref());
    }

    fn matches(&self, actual: &Self) -> bool {
        self.tx_frames == actual.tx_frames
            && self.rx_frames == actual.rx_frames
            && self.dropped == actual.dropped
            && self.duplicated == actual.duplicated
            && self.corrupted == actual.corrupted
            && self
                .tx_sha256
                .as_ref()
                .is_none_or(|expected| actual.tx_sha256.as_ref() == Some(expected))
            && self
                .rx_sha256
                .as_ref()
                .is_none_or(|expected| actual.rx_sha256.as_ref() == Some(expected))
    }
}

fn frame_digest(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn combine_frame_digests(previous: Option<String>, next: Option<&str>) -> Option<String> {
    let next = next?;
    Some(match previous {
        Some(previous) => format!(
            "{:x}",
            Sha256::digest([previous.as_bytes(), next.as_bytes()].concat())
        ),
        None => next.to_owned(),
    })
}

fn traffic_matches(
    expected: &BTreeMap<String, NetworkTraffic>,
    actual: &BTreeMap<String, NetworkTraffic>,
) -> bool {
    expected.len() == actual.len()
        && expected.iter().all(|(network, expected)| {
            actual
                .get(network)
                .is_some_and(|actual| expected.matches(actual))
        })
}

#[derive(Deserialize, Serialize)]
struct TopologyResult {
    network_sha256: String,
    #[serde(default)]
    actions: Vec<AppliedCampaignAction>,
}

/// Durable evidence that a campaign changed the topology.  This is part of
/// the replay fingerprint, rather than a host-side log that replay ignores.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct AppliedCampaignAction {
    operation: String,
    kind: String,
    target: String,
    detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AppliedFault {
    round: u64,
    kind: String,
    detail: String,
}

struct ServiceVm {
    vmm: Arc<Mutex<Vmm>>,
    event_manager: EventManager,
    storage: Vec<Arc<Mutex<Block>>>,
    networks: Vec<(String, Arc<Mutex<Net>>)>,
}

impl ServiceVm {
    fn pump(&mut self) {
        let _ = self.event_manager.run_with_timeout(0);
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .pump_simulated_devices();
    }

    fn advance_simulated_networks(&self) -> Result<(), String> {
        for (_, net) in &self.networks {
            net.lock()
                .map_err(|_| "simulated network lock poisoned".to_owned())?
                .advance_simulated_round();
        }
        Ok(())
    }
    fn exited(&self) -> Option<FcExitCode> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .shutdown_exit_code()
    }
    fn stop(&self) {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .stop(FcExitCode::Ok);
    }

    fn pause(&self) -> Result<(), String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .pause_vm()
            .map_err(|error| error.to_string())
    }

    fn resume(&self) -> Result<(), String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .resume_vm()
            .map_err(|error| error.to_string())
    }

    fn push_serial_input(&self, bytes: &[u8]) -> Result<(), String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .push_serial_input(bytes)
            .map_err(|error| error.to_string())
    }

    fn jump_virtual_time(&self, nanoseconds: u64) -> Result<(), String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .jump_virtual_time(nanoseconds)
            .map_err(|error| error.to_string())
    }

    fn virtual_time_ns(&self) -> Result<Option<Vec<u64>>, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .virtual_time_ns()
            .map_err(|error| error.to_string())
    }

    fn storage_fingerprints(
        &self,
        storage: &[StoragePlan],
    ) -> Result<BTreeMap<String, String>, String> {
        if self.storage.len() != storage.len() {
            return Err("simulated storage device count changed".to_owned());
        }
        storage
            .iter()
            .zip(&self.storage)
            .map(|(plan, block)| {
                let block = block
                    .lock()
                    .map_err(|_| "simulated storage lock poisoned".to_owned())?;
                let bytes = block
                    .simulated_bytes()
                    .ok_or_else(|| format!("storage is not simulated: {}", plan.id))?;
                Ok((plan.id.clone(), format!("{:x}", Sha256::digest(bytes))))
            })
            .collect()
    }

    fn set_network_partition(&self, network: &str, partitioned: bool) -> Result<usize, String> {
        let mut changed = 0;
        for (name, net) in &self.networks {
            if name != network {
                continue;
            }
            if !net
                .lock()
                .map_err(|_| "simulated network lock poisoned".to_owned())?
                .set_simulated_partitioned(partitioned)
            {
                return Err(format!("network is not simulated: {network}"));
            }
            changed += 1;
        }
        Ok(changed)
    }

    fn set_network_conditions(
        &self,
        network: &str,
        baseline: &NetworkConfig,
        action: Option<&CampaignAction>,
    ) -> Result<usize, String> {
        let mut changed = 0;
        for (name, net) in &self.networks {
            if name != network {
                continue;
            }
            let mut net = net
                .lock()
                .map_err(|_| "simulated network lock poisoned".to_owned())?;
            let current = net
                .sim_config()
                .ok_or_else(|| format!("network is not simulated: {network}"))?;
            let mut conditions = if action.is_some() {
                current
            } else {
                SimNetConfig {
                    seed: current.seed,
                    loopback: current.loopback,
                    drop_ppm: baseline.drop_ppm,
                    duplicate_ppm: baseline.duplicate_ppm,
                    corrupt_ppm: baseline.corrupt_ppm,
                    partitioned: current.partitioned,
                    latency_rounds: baseline.latency_rounds,
                    jitter_rounds: baseline.jitter_rounds,
                    tx_bytes_per_round: baseline.tx_bytes_per_round,
                    mtu_bytes: baseline.mtu_bytes,
                    tx_queue_frames: baseline.tx_queue_frames,
                    rx_queue_frames: baseline.rx_queue_frames,
                }
            };
            if let Some(action) = action {
                if let Some(value) = action.drop_ppm {
                    conditions.drop_ppm = value;
                }
                if let Some(value) = action.duplicate_ppm {
                    conditions.duplicate_ppm = value;
                }
                if let Some(value) = action.corrupt_ppm {
                    conditions.corrupt_ppm = value;
                }
                if let Some(value) = action.latency_rounds {
                    conditions.latency_rounds = value;
                }
                if let Some(value) = action.jitter_rounds {
                    conditions.jitter_rounds = value;
                }
                if let Some(value) = action.tx_bytes_per_round {
                    conditions.tx_bytes_per_round = value;
                }
                if let Some(value) = action.mtu_bytes {
                    conditions.mtu_bytes = value;
                }
                if let Some(value) = action.tx_queue_frames {
                    conditions.tx_queue_frames = value;
                }
                if let Some(value) = action.rx_queue_frames {
                    conditions.rx_queue_frames = value;
                }
            }
            if !net.set_simulated_conditions(conditions) {
                return Err(format!("network is not simulated: {network}"));
            }
            changed += 1;
        }
        Ok(changed)
    }

    fn network_endpoint(&self, network: &str) -> Result<Option<String>, String> {
        self.networks
            .iter()
            .find(|(name, _)| name == network)
            .map(|(_, net)| {
                net.lock()
                    .map_err(|_| "simulated network lock poisoned".to_owned())?
                    .simulated_endpoint()
                    .ok_or_else(|| format!("network is not simulated: {network}"))
            })
            .transpose()
    }

    fn set_network_link(
        &self,
        network: &str,
        destination: &str,
        blocked: bool,
    ) -> Result<(), String> {
        let (_, net) = self
            .networks
            .iter()
            .find(|(name, _)| name == network)
            .ok_or_else(|| format!("service is not on network: {network}"))?;
        if !net
            .lock()
            .map_err(|_| "simulated network lock poisoned".to_owned())?
            .set_simulated_link_blocked(destination, blocked)
        {
            return Err(format!(
                "network does not have a topology switch: {network}"
            ));
        }
        Ok(())
    }

    fn set_storage_fault(
        &self,
        storage: &[StoragePlan],
        drive: &str,
        error_ppm: u32,
        latency_rounds: u32,
        torn_write_bytes: Option<u32>,
        corrupt_read_xor: Option<u8>,
    ) -> Result<(), String> {
        let index = storage
            .iter()
            .position(|item| item.id == drive)
            .ok_or_else(|| format!("storage drive disappeared: {drive}"))?;
        let block = self
            .storage
            .get(index)
            .ok_or_else(|| format!("simulated storage disappeared: {drive}"))?;
        if !block
            .lock()
            .map_err(|_| "simulated storage lock poisoned".to_owned())?
            .set_simulated_faults(
                error_ppm,
                latency_rounds,
                torn_write_bytes,
                corrupt_read_xor,
            )
        {
            return Err(format!("storage is not simulated: {drive}"));
        }
        Ok(())
    }

    fn network_traffic(&self) -> Result<BTreeMap<String, NetworkTraffic>, String> {
        self.networks
            .iter()
            .map(|(name, net)| {
                let net = net
                    .lock()
                    .map_err(|_| "simulated network lock poisoned".to_owned())?;
                let stats = net
                    .simulated_stats()
                    .ok_or_else(|| format!("network is not simulated: {name}"))?;
                Ok((
                    name.clone(),
                    NetworkTraffic {
                        tx_frames: stats.tx_frames,
                        rx_frames: stats.rx_frames,
                        dropped: stats.dropped,
                        duplicated: stats.duplicated,
                        corrupted: stats.corrupted,
                        tx_sha256: Some(frame_digest(stats.tx_sha256)),
                        rx_sha256: Some(frame_digest(stats.rx_sha256)),
                    },
                ))
            })
            .collect()
    }

    fn network_trace(&self) -> Result<BTreeMap<String, Vec<NetworkFrame>>, String> {
        self.networks
            .iter()
            .map(|(name, net)| {
                let net = net
                    .lock()
                    .map_err(|_| "simulated network lock poisoned".to_owned())?;
                let trace = net
                    .simulated_trace()
                    .ok_or_else(|| format!("network is not simulated: {name}"))?;
                Ok((
                    name.clone(),
                    trace
                        .into_iter()
                        .map(|frame| NetworkFrame {
                            round: frame.round,
                            direction: match frame.direction {
                                SimNetFrameDirection::Tx => "tx",
                                SimNetFrameDirection::Rx => "rx",
                                SimNetFrameDirection::Drop => "drop",
                            }
                            .to_owned(),
                            drop_reason: frame.drop_reason.map(|reason| {
                                match reason {
                                    SimNetDropReason::Mtu => "mtu",
                                    SimNetDropReason::Partition => "partition",
                                    SimNetDropReason::LinkPartition => "link_partition",
                                    SimNetDropReason::RandomLoss => "random_loss",
                                    SimNetDropReason::TransmitQueue => "tx_queue",
                                    SimNetDropReason::ReceiveQueue => "rx_queue",
                                    SimNetDropReason::ReceiveBuffer => "rx_buffer",
                                }
                                .to_owned()
                            }),
                            data_hex: hex(&frame.bytes),
                        })
                        .collect(),
                ))
            })
            .collect()
    }
}

struct ServiceRuntime {
    vm: ServiceVm,
    serial_logs: Vec<PathBuf>,
    next_fault: usize,
    paused_until: Option<u64>,
    faults: Vec<AppliedFault>,
    network_traffic: BTreeMap<String, NetworkTraffic>,
    network_trace: BTreeMap<String, Vec<NetworkFrame>>,
}

impl ServiceRuntime {
    fn record_network_traffic(&mut self) -> Result<(), String> {
        for (name, traffic) in self.vm.network_traffic()? {
            self.network_traffic.entry(name).or_default().add(&traffic);
        }
        Ok(())
    }

    fn record_network_trace(&mut self) -> Result<(), String> {
        for (name, trace) in self.vm.network_trace()? {
            self.network_trace.entry(name).or_default().extend(trace);
        }
        Ok(())
    }
}

fn main() -> std::process::ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("theseus-topology: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let (flag_plan, plan, flag_output, output, minimize) = match args.as_slice() {
        [flag_plan, plan, flag_output, output] => (flag_plan, plan, flag_output, output, false),
        [flag_plan, plan, flag_output, output, flag] if flag == "--minimize" => {
            (flag_plan, plan, flag_output, output, true)
        }
        _ => return Err(USAGE.to_owned()),
    };
    if flag_plan != "--plan" || flag_output != "--output" {
        return Err(USAGE.to_owned());
    }
    let input = fs::read_to_string(plan).map_err(|error| format!("cannot read {plan}: {error}"))?;
    let topology: TopologyPlan = serde_json::from_str(&input)
        .map_err(|error| format!("cannot parse topology plan: {error}"))?;
    if topology.format != "theseus-compose-plan-v1" || topology.services.is_empty() {
        return Err("unsupported or empty topology plan".to_owned());
    }
    let service_names = topology.services.keys().cloned().collect::<Vec<_>>();
    let expected_serial = recorded_serial_fingerprints(Path::new(plan), &service_names)?;
    let expected_faults = recorded_fault_fingerprints(Path::new(plan), &service_names)?;
    let expected_network = recorded_network_fingerprint(Path::new(plan))?;
    let expected_actions = recorded_campaign_actions(Path::new(plan))?;
    let expected_storage = recorded_storage_fingerprints(Path::new(plan), &service_names)?;
    let expected_traffic = recorded_network_traffic(Path::new(plan), &service_names)?;
    let expected_virtual_time = recorded_virtual_times(Path::new(plan), &service_names)?;
    let output = PathBuf::from(output);
    if output.exists() {
        return Err(format!(
            "replay output already exists: {}",
            output.display()
        ));
    }
    fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    if topology.campaign.is_some() {
        if expected_serial.is_some()
            || expected_faults.is_some()
            || expected_network.is_some()
            || expected_actions.is_some()
            || expected_storage.is_some()
            || expected_traffic.is_some()
            || expected_virtual_time.is_some()
        {
            return Err(
                "campaign bundles replay their recorded schedules, not single-run fingerprints"
                    .to_owned(),
            );
        }
        if minimize {
            execute_campaign_minimized(topology, &output, Path::new(plan))
        } else {
            execute_campaign(topology, &output)
        }
    } else {
        if minimize {
            return Err("--minimize requires a campaign replay bundle".to_owned());
        }
        execute(
            topology,
            &output,
            expected_serial,
            expected_faults,
            expected_network,
            expected_actions,
            expected_storage,
            expected_traffic,
            expected_virtual_time,
        )
    }
}

/// Execute an autonomous campaign as a deterministic corpus of complete
/// topology timelines.  A topology service is the workload driver: Theseus
/// writes operation bytes to its UART, while every service stays inside the
/// same simulated switch and fault scheduler.  Starting complete timelines is
/// deliberately the initial checkpoint representation: every retained run is
/// a locked, independently replayable topology bundle, rather than a host
/// process whose state cannot be reproduced.
fn execute_campaign(mut topology: TopologyPlan, output: &Path) -> Result<(), String> {
    let campaign = topology
        .campaign
        .take()
        .expect("campaign execution requires a campaign");
    let base = serde_json::to_vec(&topology)
        .map_err(|error| format!("cannot encode campaign base plan: {error}"))?;
    let schedules = campaign_schedules(&campaign);
    if schedules.is_empty() {
        return Err("campaign produced no schedules".to_owned());
    }
    fs::create_dir_all(output.join("runs")).map_err(|error| error.to_string())?;
    let mut runs = Vec::new();
    let mut seen_markers = std::collections::BTreeSet::new();
    for (index, schedule) in schedules.iter().enumerate() {
        let mut run: TopologyPlan = serde_json::from_slice(&base)
            .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
        apply_campaign_schedule(&mut run, &campaign, schedule)?;
        let run_dir = output.join("runs").join(format!("{index:03}"));
        let status = execute(run, &run_dir, None, None, None, None, None, None, None);
        let markers = campaign_markers(&run_dir)?;
        let actions = campaign_actions(&run_dir)?;
        let novelty = markers
            .into_iter()
            .filter(|marker| seen_markers.insert(marker.clone()))
            .collect::<Vec<_>>();
        runs.push(CampaignRun {
            index,
            operations: schedule
                .operations
                .iter()
                .map(|operation| campaign.operations[*operation].name.clone())
                .collect(),
            fault: (schedule.faults.len() == 1)
                .then(|| campaign_fault_name(&campaign.faults[schedule.faults[0]])),
            faults: campaign_fault_names(&campaign, &schedule.faults),
            actions,
            status: if status.is_ok() { "passed" } else { "failed" },
            novelty,
        });
    }
    let properties = evaluate_campaign_properties(&campaign, output, &runs)?;
    let passed = runs.iter().all(|run| run.status == "passed")
        && properties
            .iter()
            .all(|property| property.status == "passed");
    let first_plan = output.join("runs/000/replay-plan.json");
    let mut replay: TopologyPlan = serde_json::from_slice(
        &fs::read(&first_plan)
            .map_err(|error| format!("cannot read {}: {error}", first_plan.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", first_plan.display()))?;
    replay.campaign = Some(campaign);
    fs::write(
        output.join("replay-plan.json"),
        serde_json::to_vec_pretty(&replay).expect("campaign replay plan serializes"),
    )
    .map_err(|error| error.to_string())?;
    fs::write(
        output.join("campaign-result.json"),
        serde_json::to_vec_pretty(&CampaignResult {
            format: "theseus-compose-campaign-result-v1",
            status: if passed { "passed" } else { "failed" },
            driver: replay
                .campaign
                .as_ref()
                .expect("campaign remains in replay plan")
                .driver
                .clone(),
            runs,
            properties,
        })
        .expect("campaign result serializes"),
    )
    .map_err(|error| error.to_string())?;
    if passed {
        Ok(())
    } else {
        Err(format!(
            "campaign found a failing timeline; inspect {}",
            output.display()
        ))
    }
}

fn campaign_actions(run: &Path) -> Result<Vec<AppliedCampaignAction>, String> {
    let result_path = run.join("topology-result.json");
    let result = fs::read(&result_path)
        .map_err(|error| format!("cannot read {}: {error}", result_path.display()))?;
    serde_json::from_slice::<TopologyResult>(&result)
        .map(|result| result.actions)
        .map_err(|error| format!("cannot parse {}: {error}", result_path.display()))
}

/// Delta-debug the first timeline that violates an individual property.  The
/// reducer removes one operation at a time and re-executes the complete,
/// locked topology.  It does not pretend that `sometimes` and `reachable`
/// failures have a single counterexample: those properties fail because the
/// corpus has no witness, so their full first schedule is retained.
fn execute_campaign_minimized(
    mut topology: TopologyPlan,
    output: &Path,
    source_plan: &Path,
) -> Result<(), String> {
    let campaign = topology
        .campaign
        .take()
        .expect("campaign minimization requires a campaign");
    let source = source_plan
        .parent()
        .ok_or_else(|| format!("campaign plan has no parent: {}", source_plan.display()))?;
    let recorded: RecordedCampaignResult = serde_json::from_slice(
        &fs::read(source.join("campaign-result.json"))
            .map_err(|error| format!("cannot read campaign result: {error}"))?,
    )
    .map_err(|error| format!("cannot parse campaign result: {error}"))?;
    let base = serde_json::to_vec(&topology)
        .map_err(|error| format!("cannot encode campaign base plan: {error}"))?;
    let (property, mut schedule) = campaign_counterexample(&campaign, source, &recorded)?;
    let original_operations = schedule
        .operations
        .iter()
        .map(|operation| campaign.operations[*operation].name.clone())
        .collect::<Vec<_>>();
    let attempts = output.join("minimization-attempts");
    fs::create_dir_all(&attempts).map_err(|error| error.to_string())?;
    let mut attempt = 0_usize;
    let mut index = 0;
    while index < schedule.operations.len() {
        let mut candidate = CampaignSchedule {
            operations: schedule.operations.clone(),
            faults: schedule.faults.clone(),
        };
        candidate.operations.remove(index);
        if candidate.operations.is_empty() {
            index += 1;
            continue;
        }
        let directory = attempts.join(format!("{attempt:03}"));
        attempt += 1;
        let mut plan: TopologyPlan = serde_json::from_slice(&base)
            .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
        apply_campaign_schedule(&mut plan, &campaign, &candidate)?;
        let _ = execute(plan, &directory, None, None, None, None, None, None, None);
        if property_fails_in_run(&property, &directory) {
            schedule = candidate;
        } else {
            index += 1;
        }
    }
    let mut final_plan: TopologyPlan = serde_json::from_slice(&base)
        .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
    apply_campaign_schedule(&mut final_plan, &campaign, &schedule)?;
    add_counterexample_check(&mut final_plan, &campaign, &property)?;
    execute(final_plan, output, None, None, None, None, None, None, None)?;
    let minimized_operations = schedule
        .operations
        .iter()
        .map(|operation| campaign.operations[*operation].name.clone())
        .collect::<Vec<_>>();
    fs::write(
        output.join("minimization.json"),
        serde_json::to_vec_pretty(&CampaignMinimization {
            property: property.name.clone(),
            original_operations,
            minimized_operations,
            fault: (schedule.faults.len() == 1)
                .then(|| campaign_fault_name(&campaign.faults[schedule.faults[0]])),
            faults: campaign_fault_names(&campaign, &schedule.faults),
        })
        .expect("campaign minimization serializes"),
    )
    .map_err(|error| error.to_string())?;
    if property_fails_in_run(&property, output) {
        Err(format!(
            "minimized campaign counterexample reproduced; replay it with `theseus compose replay {}`",
            output.display()
        ))
    } else {
        Err("campaign counterexample did not reproduce during minimization".to_owned())
    }
}

fn add_counterexample_check(
    topology: &mut TopologyPlan,
    campaign: &CampaignPlan,
    property: &CampaignProperty,
) -> Result<(), String> {
    let service = property.service.as_ref().unwrap_or(&campaign.driver);
    let check = topology
        .services
        .get_mut(service)
        .ok_or_else(|| format!("property service disappeared: {service}"))?;
    let kind = match property.kind {
        PropertyKind::Unreachable => CheckKind::SerialContains,
        PropertyKind::Always | PropertyKind::Sometimes | PropertyKind::Reachable => {
            CheckKind::SerialNotContains
        }
    };
    check.run.checks.push(CheckPlan {
        name: format!("counterexample: {}", property.name),
        kind,
        value: property.contains.clone(),
    });
    Ok(())
}

fn campaign_counterexample(
    campaign: &CampaignPlan,
    source: &Path,
    recorded: &RecordedCampaignResult,
) -> Result<(CampaignProperty, CampaignSchedule), String> {
    for property in &campaign.properties {
        let matches = recorded
            .runs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                property_matches_in_run(property, &source.join("runs").join(format!("{index:03}")))
            })
            .collect::<Vec<_>>();
        let property_failed = match property.kind {
            PropertyKind::Always => matches.iter().any(|matched| !matched),
            PropertyKind::Unreachable => matches.iter().any(|matched| *matched),
            PropertyKind::Sometimes | PropertyKind::Reachable => {
                matches.iter().all(|matched| !matched)
            }
        };
        if !property_failed {
            continue;
        }
        let index = match property.kind {
            PropertyKind::Always => matches.iter().position(|matched| !matched),
            PropertyKind::Unreachable => matches.iter().position(|matched| *matched),
            PropertyKind::Sometimes | PropertyKind::Reachable => Some(0),
        }
        .expect("failed property has a recorded run");
        let recorded_run = &recorded.runs[index];
        let operations = recorded_run
            .operations
            .iter()
            .map(|name| {
                campaign
                    .operations
                    .iter()
                    .position(|operation| operation.name == *name)
                    .ok_or_else(|| format!("recorded operation is no longer declared: {name}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let names = if recorded_run.faults.is_empty() {
            recorded_run.fault.iter().cloned().collect()
        } else {
            recorded_run.faults.clone()
        };
        let faults = names
            .iter()
            .map(|name| {
                campaign
                    .faults
                    .iter()
                    .position(|fault| campaign_fault_name(fault) == *name)
                    .ok_or_else(|| format!("recorded fault is no longer declared: {name}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok((
            CampaignProperty {
                name: property.name.clone(),
                kind: property.kind,
                contains: property.contains.clone(),
                service: property.service.clone(),
            },
            CampaignSchedule { operations, faults },
        ));
    }
    Err("campaign bundle has no failing property to minimize".to_owned())
}

fn property_fails_in_run(property: &CampaignProperty, run: &Path) -> bool {
    let matched = property_matches_in_run(property, run);
    match property.kind {
        PropertyKind::Always => !matched,
        PropertyKind::Unreachable => matched,
        PropertyKind::Sometimes | PropertyKind::Reachable => !matched,
    }
}

fn property_matches_in_run(property: &CampaignProperty, run: &Path) -> bool {
    let services = property
        .service
        .as_ref()
        .map(|service| vec![service.clone()])
        .unwrap_or_else(|| {
            fs::read_dir(run.join("services"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        });
    services
        .into_iter()
        .any(|service| campaign_serial_contains(run, &service, property.contains.as_bytes()))
}

#[derive(Debug)]
struct CampaignSchedule {
    operations: Vec<usize>,
    faults: Vec<usize>,
}

fn campaign_schedules(campaign: &CampaignPlan) -> Vec<CampaignSchedule> {
    let mut operations = Vec::new();
    // Deterministic breadth-first operation histories.  This gives each
    // operation a single-step attempt before longer histories consume budget.
    for depth in 1..=campaign.operations.len() {
        for start in 0..campaign.operations.len() {
            let history = (0..depth)
                .map(|offset| (start + offset) % campaign.operations.len())
                .collect::<Vec<_>>();
            operations.push(history);
        }
    }
    let mut schedules = Vec::new();
    for history in operations {
        schedules.push(CampaignSchedule {
            operations: history.clone(),
            faults: Vec::new(),
        });
        let applicable = campaign
            .faults
            .iter()
            .enumerate()
            .filter_map(|(index, fault)| {
                campaign_fault_applies(fault, &history, campaign).then_some(index)
            })
            .collect::<Vec<_>>();
        for faults in campaign_fault_combinations(
            &campaign.faults,
            &applicable,
            usize::from(campaign.max_faults_per_run),
        ) {
            schedules.push(CampaignSchedule {
                operations: history.clone(),
                faults,
            });
        }
    }
    schedules.truncate(usize::from(campaign.max_runs));
    schedules
}

/// Generate combinations in declaration order. The operation history supplies
/// the timeline ordering for barrier actions, so a combination represents one
/// complete failure/recovery scenario rather than host-side interleaving.
fn campaign_fault_combinations(
    faults: &[CampaignFault],
    applicable: &[usize],
    maximum: usize,
) -> Vec<Vec<usize>> {
    fn visit(
        faults: &[CampaignFault],
        applicable: &[usize],
        maximum: usize,
        start: usize,
        selected: &mut Vec<usize>,
        output: &mut Vec<Vec<usize>>,
    ) {
        if !selected.is_empty() {
            output.push(selected.clone());
        }
        if selected.len() == maximum {
            return;
        }
        for (offset, index) in applicable.iter().enumerate().skip(start) {
            if selected
                .iter()
                .all(|chosen| campaign_faults_compatible(&faults[*chosen], &faults[*index]))
            {
                selected.push(*index);
                visit(faults, applicable, maximum, offset + 1, selected, output);
                selected.pop();
            }
        }
    }

    let mut output = Vec::new();
    visit(faults, applicable, maximum, 0, &mut Vec::new(), &mut output);
    output
}

fn campaign_faults_compatible(first: &CampaignFault, second: &CampaignFault) -> bool {
    let lifecycle = |fault: &CampaignFault| {
        matches!(
            fault.kind,
            CampaignFaultKind::Pause | CampaignFaultKind::Restart | CampaignFaultKind::ClockJump
        )
    };
    if !lifecycle(first) || !lifecycle(second) || first.service != second.service {
        return true;
    }
    let first_round = first.at_round.expect("validated lifecycle round");
    let second_round = second.at_round.expect("validated lifecycle round");
    if first_round == second_round {
        return false;
    }
    let (earlier, later) = if first_round < second_round {
        (first, second_round)
    } else {
        (second, first_round)
    };
    !matches!(earlier.kind, CampaignFaultKind::Pause)
        || later
            >= earlier
                .at_round
                .expect("validated lifecycle round")
                .saturating_add(earlier.duration_rounds.expect("validated pause duration"))
}

fn campaign_fault_applies(
    fault: &CampaignFault,
    history: &[usize],
    campaign: &CampaignPlan,
) -> bool {
    match fault.kind {
        CampaignFaultKind::Pause | CampaignFaultKind::Restart | CampaignFaultKind::ClockJump => {
            true
        }
        CampaignFaultKind::Partition
        | CampaignFaultKind::Heal
        | CampaignFaultKind::LinkPartition
        | CampaignFaultKind::LinkHeal
        | CampaignFaultKind::StorageFault
        | CampaignFaultKind::StorageRecover
        | CampaignFaultKind::NetworkFault
        | CampaignFaultKind::NetworkRecover => {
            let Some(after) = &fault.after else {
                return false;
            };
            history
                .iter()
                .any(|operation| campaign.operations[*operation].name == *after)
        }
    }
}

fn apply_campaign_schedule(
    topology: &mut TopologyPlan,
    campaign: &CampaignPlan,
    schedule: &CampaignSchedule,
) -> Result<(), String> {
    let selected = schedule
        .faults
        .iter()
        .map(|index| &campaign.faults[*index])
        .collect::<Vec<_>>();
    let events = schedule
        .operations
        .iter()
        .map(|operation| {
            let operation = &campaign.operations[*operation];
            let actions = selected
                .iter()
                .filter(|candidate| candidate.after.as_deref() == Some(&operation.name))
                .map(|candidate| campaign_action(candidate))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(EventPlan {
                data_hex: operation.input_hex.clone(),
                checkpoint: Some(format!("THES:CHECKPOINT:{}", operation.name)),
                actions,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let driver = topology
        .services
        .get_mut(&campaign.driver)
        .ok_or_else(|| format!("campaign driver disappeared: {}", campaign.driver))?;
    driver.run.events = events;
    for candidate in selected {
        if matches!(
            candidate.kind,
            CampaignFaultKind::Pause | CampaignFaultKind::Restart | CampaignFaultKind::ClockJump
        ) {
            let service = candidate
                .service
                .as_ref()
                .expect("validated lifecycle service");
            let at_round = candidate.at_round.expect("validated lifecycle round");
            let kind = match candidate.kind {
                CampaignFaultKind::Pause => FaultKind::Pause,
                CampaignFaultKind::Restart => FaultKind::Restart,
                CampaignFaultKind::ClockJump => FaultKind::ClockJump,
                _ => unreachable!(),
            };
            let target = topology
                .services
                .get_mut(service)
                .ok_or_else(|| format!("campaign fault service disappeared: {service}"))?;
            target.faults.push(FaultPlan {
                at_round,
                kind,
                duration_rounds: candidate.duration_rounds,
                nanoseconds: candidate.nanoseconds,
            });
            target.faults.sort_by_key(|fault| fault.at_round);
        }
    }
    Ok(())
}

fn campaign_action(fault: &CampaignFault) -> Result<CampaignAction, String> {
    let operation = fault
        .after
        .clone()
        .ok_or_else(|| "campaign topology action has no operation barrier".to_owned())?;
    Ok(CampaignAction {
        operation,
        kind: fault.kind,
        service: fault.service.clone(),
        network: fault.network.clone(),
        from: fault.from.clone(),
        to: fault.to.clone(),
        drive: fault.drive.clone(),
        error_ppm: fault.error_ppm,
        latency_rounds: fault.latency_rounds,
        torn_write_bytes: fault.torn_write_bytes,
        corrupt_read_xor: fault.corrupt_read_xor,
        drop_ppm: fault.drop_ppm,
        duplicate_ppm: fault.duplicate_ppm,
        corrupt_ppm: fault.corrupt_ppm,
        jitter_rounds: fault.jitter_rounds,
        tx_bytes_per_round: fault.tx_bytes_per_round,
        mtu_bytes: fault.mtu_bytes,
        tx_queue_frames: fault.tx_queue_frames,
        rx_queue_frames: fault.rx_queue_frames,
    })
}

fn campaign_fault_name(fault: &CampaignFault) -> String {
    match fault.kind {
        CampaignFaultKind::Pause | CampaignFaultKind::Restart | CampaignFaultKind::ClockJump => {
            let kind = match fault.kind {
                CampaignFaultKind::Pause => "pause",
                CampaignFaultKind::Restart => "restart",
                CampaignFaultKind::ClockJump => "clock_jump",
                _ => unreachable!(),
            };
            format!(
                "{}:{kind}@{}",
                fault
                    .service
                    .as_deref()
                    .expect("validated lifecycle service"),
                fault.at_round.expect("validated lifecycle round")
            )
        }
        CampaignFaultKind::Partition | CampaignFaultKind::Heal => format!(
            "{}:{}@{}",
            fault.network.as_deref().expect("validated action network"),
            match fault.kind {
                CampaignFaultKind::Partition => "partition",
                CampaignFaultKind::Heal => "heal",
                _ => unreachable!(),
            },
            fault.after.as_deref().expect("validated action operation")
        ),
        CampaignFaultKind::LinkPartition | CampaignFaultKind::LinkHeal => format!(
            "{}:{}->{}:{}@{}",
            fault.network.as_deref().expect("validated action network"),
            fault.from.as_deref().expect("validated action source"),
            fault.to.as_deref().expect("validated action destination"),
            match fault.kind {
                CampaignFaultKind::LinkPartition => "link_partition",
                CampaignFaultKind::LinkHeal => "link_heal",
                _ => unreachable!(),
            },
            fault.after.as_deref().expect("validated action operation")
        ),
        CampaignFaultKind::StorageFault | CampaignFaultKind::StorageRecover => format!(
            "{}:{}:{}@{}",
            fault.service.as_deref().expect("validated storage service"),
            fault.drive.as_deref().expect("validated storage drive"),
            match fault.kind {
                CampaignFaultKind::StorageFault => "storage_fault",
                CampaignFaultKind::StorageRecover => "storage_recover",
                _ => unreachable!(),
            },
            fault.after.as_deref().expect("validated action operation")
        ),
        CampaignFaultKind::NetworkFault | CampaignFaultKind::NetworkRecover => format!(
            "{}:{}@{}",
            fault.network.as_deref().expect("validated action network"),
            match fault.kind {
                CampaignFaultKind::NetworkFault => "network_fault",
                CampaignFaultKind::NetworkRecover => "network_recover",
                _ => unreachable!(),
            },
            fault.after.as_deref().expect("validated action operation")
        ),
    }
}

fn campaign_fault_names(campaign: &CampaignPlan, faults: &[usize]) -> Vec<String> {
    faults
        .iter()
        .map(|index| campaign_fault_name(&campaign.faults[*index]))
        .collect()
}

fn campaign_markers(run: &Path) -> Result<Vec<String>, String> {
    let mut markers = std::collections::BTreeSet::new();
    let services = fs::read_dir(run.join("services")).map_err(|error| error.to_string())?;
    for service in services {
        let service = service.map_err(|error| error.to_string())?;
        for log in fs::read_dir(service.path()).map_err(|error| error.to_string())? {
            let log = log.map_err(|error| error.to_string())?;
            let name = log.file_name();
            let name = name.to_string_lossy();
            if name == "serial.log" || (name.starts_with("serial-") && name.ends_with(".log")) {
                let text = fs::read_to_string(log.path()).unwrap_or_default();
                for line in text.lines() {
                    if let Some(marker) = line.trim().strip_prefix("THES:M:") {
                        markers.insert(marker.to_owned());
                    }
                }
            }
        }
    }
    Ok(markers.into_iter().collect())
}

fn evaluate_campaign_properties(
    campaign: &CampaignPlan,
    output: &Path,
    runs: &[CampaignRun],
) -> Result<Vec<CampaignPropertyResult>, String> {
    campaign
        .properties
        .iter()
        .map(|property| {
            let matches = runs
                .iter()
                .map(|run| {
                    let services = property
                        .service
                        .as_ref()
                        .map(|service| vec![service.clone()])
                        .unwrap_or_else(|| campaign_services(output, run.index));
                    services.into_iter().any(|service| {
                        campaign_serial_contains(
                            &output.join("runs").join(format!("{:03}", run.index)),
                            &service,
                            property.contains.as_bytes(),
                        )
                    })
                })
                .collect::<Vec<_>>();
            let found = matches.iter().filter(|matched| **matched).count();
            let passed = match property.kind {
                PropertyKind::Always => found == runs.len(),
                PropertyKind::Sometimes | PropertyKind::Reachable => found > 0,
                PropertyKind::Unreachable => found == 0,
            };
            let kind = match property.kind {
                PropertyKind::Always => "always",
                PropertyKind::Sometimes => "sometimes",
                PropertyKind::Reachable => "reachable",
                PropertyKind::Unreachable => "unreachable",
            };
            Ok(CampaignPropertyResult {
                name: property.name.clone(),
                kind,
                status: if passed { "passed" } else { "failed" },
                detail: format!(
                    "{} of {} retained timelines contained {:?}",
                    found,
                    runs.len(),
                    property.contains
                ),
            })
        })
        .collect()
}

fn campaign_services(output: &Path, run: usize) -> Vec<String> {
    fs::read_dir(
        output
            .join("runs")
            .join(format!("{run:03}"))
            .join("services"),
    )
    .into_iter()
    .flatten()
    .filter_map(Result::ok)
    .map(|entry| entry.file_name().to_string_lossy().into_owned())
    .collect()
}

fn campaign_serial_contains(run: &Path, service: &str, needle: &[u8]) -> bool {
    fs::read_dir(run.join("services").join(service))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == "serial.log" || (name.starts_with("serial-") && name.ends_with(".log"))
        })
        .any(|entry| {
            fs::read(entry.path())
                .unwrap_or_default()
                .windows(needle.len())
                .any(|value| value == needle)
        })
}

fn execute(
    mut topology: TopologyPlan,
    output: &Path,
    expected_serial: Option<BTreeMap<String, Vec<String>>>,
    expected_faults: Option<BTreeMap<String, String>>,
    expected_network: Option<String>,
    expected_actions: Option<Vec<AppliedCampaignAction>>,
    expected_storage: Option<BTreeMap<String, BTreeMap<String, String>>>,
    expected_traffic: Option<BTreeMap<String, BTreeMap<String, NetworkTraffic>>>,
    expected_virtual_time: Option<BTreeMap<String, Option<Vec<u64>>>>,
) -> Result<(), String> {
    if let Some(runner) = &mut topology.topology_runner {
        fs::create_dir_all(output.join("artifacts")).map_err(|error| error.to_string())?;
        let locked = lock_artifact(output, "theseus-topology", runner)?;
        runner.path = fs::canonicalize(locked)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
    }
    let mut switches: BTreeMap<String, SharedSimSwitch> = topology
        .networks
        .keys()
        .map(|name| (name.clone(), Arc::new(Mutex::new(SimSwitch::new()))))
        .collect();
    let mut services = BTreeMap::new();
    let names = topology.services.keys().cloned().collect::<Vec<_>>();
    for name in &names {
        let service = &topology.services[name];
        let service_dir = output.join("services").join(name);
        fs::create_dir_all(service_dir.join("artifacts")).map_err(|error| error.to_string())?;
        let kernel = lock_artifact(&service_dir, "kernel", &service.run.guest.kernel)?;
        let initramfs = lock_artifact(&service_dir, "initramfs", &service.run.guest.initramfs)?;
        let runtime = lock_artifact(
            &service_dir,
            "firecracker",
            &service.run.runtime.firecracker,
        )?;
        let locked = topology
            .services
            .get_mut(name)
            .expect("topology service missing");
        locked.run.runtime.firecracker.path = fs::canonicalize(runtime)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
        locked.run.guest.kernel.path = fs::canonicalize(kernel)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
        locked.run.guest.initramfs.path = fs::canonicalize(initramfs)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
    }
    fs::write(
        output.join("replay-plan.json"),
        serde_json::to_vec_pretty(&topology).unwrap(),
    )
    .map_err(|error| error.to_string())?;
    for name in &names {
        let service = &topology.services[name];
        let service_dir = output.join("services").join(name);
        let serial = service_dir.join("serial.log");
        let vm = build_service(
            name,
            0,
            service,
            Path::new(&service.run.guest.kernel.path),
            Path::new(&service.run.guest.initramfs.path),
            &serial,
            &mut switches,
        )?;
        services.insert(
            name.clone(),
            ServiceRuntime {
                vm,
                serial_logs: vec![serial],
                next_fault: 0,
                paused_until: None,
                faults: Vec::new(),
                network_traffic: BTreeMap::new(),
                network_trace: BTreeMap::new(),
            },
        );
    }
    for name in &names {
        let service = &services[name];
        service.vm.resume()?;
    }
    let mut actions = Vec::new();
    for name in &names {
        let events = topology.services[name].run.events.clone();
        if events.iter().all(|event| event.actions.is_empty()) {
            let service = &services[name];
            inject_serial_events(&service.vm, &events, &service.serial_logs[0])?;
            continue;
        }
        let mut driver = services.remove(name).expect("topology service missing");
        let serial = driver.serial_logs[0].clone();
        inject_campaign_events(
            name,
            &mut driver,
            &events,
            &serial,
            &topology,
            &mut services,
            &mut actions,
        )?;
        services.insert(name.clone(), driver);
    }
    let timeout = topology
        .services
        .values()
        .map(|service| service.run.run.timeout_secs)
        .max()
        .unwrap_or(1);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut round = 0;
    while Instant::now() < deadline
        && services
            .values()
            .any(|service| service.vm.exited().is_none())
    {
        round += 1;
        for name in topology.services.keys() {
            let mut service = services.remove(name).expect("topology service missing");
            apply_scheduled_faults(
                round,
                name,
                &topology.services[name],
                &output.join("services").join(name),
                &mut service,
                &mut switches,
            )?;
            if service.paused_until.is_none() && service.vm.exited().is_none() {
                service.vm.pump();
            }
            services.insert(name.clone(), service);
        }
        advance_network_round(&switches, &services)?;
    }
    let network_sha256 = network_fingerprint(&switches)?;
    fs::write(
        output.join("topology-result.json"),
        serde_json::to_vec_pretty(&TopologyResult {
            network_sha256: network_sha256.clone(),
            actions: actions.clone(),
        })
        .unwrap(),
    )
    .map_err(|error| error.to_string())?;
    let mut failed = false;
    if let Some(expected) = &expected_actions {
        if expected != &actions {
            failed = true;
            fs::write(
                output.join("replay-actions-mismatch.txt"),
                "campaign topology actions differ from the original replay bundle\n",
            )
            .map_err(|error| error.to_string())?;
        }
    }
    for (name, service) in &mut services {
        service.record_network_traffic()?;
        service.record_network_trace()?;
        let exit = service.vm.exited();
        let (exit_status, mut error) = match exit {
            Some(FcExitCode::Ok) => ("passed", None),
            Some(code) => ("failed", Some(format!("guest exited with {code:?}"))),
            None => (
                "failed",
                Some("guest did not exit before topology timeout".to_owned()),
            ),
        };
        let mut checks = evaluate_checks(&topology.services[name].run.checks, &service.serial_logs);
        let serial_sha256 = serial_fingerprints(&service.serial_logs)?;
        let faults_sha256 = fault_fingerprint(&service.faults)?;
        let storage_sha256 = service
            .vm
            .storage_fingerprints(&topology.services[name].run.storage)?;
        let virtual_time_ns = service.vm.virtual_time_ns()?;
        checks.insert(
            0,
            CheckResult {
                name: "guest_exit".to_owned(),
                status: exit_status,
                detail: error
                    .clone()
                    .unwrap_or_else(|| "guest exited with status 0".to_owned()),
            },
        );
        if let Some(expected) = &expected_serial {
            let expected = expected
                .get(name)
                .expect("recorded serial fingerprints missing service");
            let matches = expected == &serial_sha256;
            checks.push(CheckResult {
                name: "replay_serial".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "serial logs match the original replay bundle".to_owned()
                } else {
                    "serial logs differ from the original replay bundle".to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some("serial replay fingerprint changed".to_owned());
            }
        }
        if let Some(expected) = &expected_faults {
            let expected = expected
                .get(name)
                .expect("recorded fault fingerprint missing service");
            let matches = expected == &faults_sha256;
            checks.push(CheckResult {
                name: "replay_faults".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "applied faults match the original replay bundle".to_owned()
                } else {
                    "applied faults differ from the original replay bundle".to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some("fault replay fingerprint changed".to_owned());
            }
        }
        if let Some(expected) = &expected_network {
            let matches = expected == &network_sha256;
            checks.push(CheckResult {
                name: "replay_network".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "network topology matches the original replay bundle".to_owned()
                } else {
                    "network topology differs from the original replay bundle".to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some("network replay fingerprint changed".to_owned());
            }
        }
        if let Some(expected) = &expected_storage {
            let expected = expected
                .get(name)
                .expect("recorded storage fingerprint missing service");
            let matches = expected == &storage_sha256;
            checks.push(CheckResult {
                name: "replay_storage".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "simulated storage matches the original replay bundle".to_owned()
                } else {
                    "simulated storage differs from the original replay bundle".to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some("storage replay fingerprint changed".to_owned());
            }
        }
        if let Some(expected) = &expected_traffic {
            let expected = expected
                .get(name)
                .expect("recorded network traffic missing service");
            let matches = traffic_matches(expected, &service.network_traffic);
            checks.push(CheckResult {
                name: "replay_network_traffic".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "simulated network traffic matches the original replay bundle, including payload fingerprints".to_owned()
                } else {
                    "simulated network traffic differs from the original replay bundle, including payload fingerprints".to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some("network traffic or payload replay fingerprint changed".to_owned());
            }
        }
        if let Some(expected) = &expected_virtual_time {
            let expected = expected
                .get(name)
                .expect("recorded virtual time missing service");
            let matches = expected == &virtual_time_ns;
            checks.push(CheckResult {
                name: "replay_virtual_time".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "virtual clock state matches the original replay bundle".to_owned()
                } else {
                    "virtual clock state differs from the original replay bundle".to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some("virtual time replay fingerprint changed".to_owned());
            }
        }
        let status = if checks.iter().all(|check| check.status == "passed") {
            "passed"
        } else {
            "failed"
        };
        if status == "failed" {
            failed = true;
        }
        let result = ServiceResult {
            status,
            serial_log: service.serial_logs[0].display().to_string(),
            serial_logs: service
                .serial_logs
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            serial_sha256,
            faults_sha256,
            storage_sha256,
            network_traffic: service.network_traffic.clone(),
            network_trace: service.network_trace.clone(),
            virtual_time_ns,
            error,
            checks,
            faults: service.faults.clone(),
        };
        fs::write(
            output.join("services").join(name).join("result.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .map_err(|error| error.to_string())?;
        service.vm.stop();
    }
    if failed {
        Err(format!(
            "one or more services failed; inspect {}/services",
            output.display()
        ))
    } else {
        Ok(())
    }
}

fn recorded_network_fingerprint(plan: &Path) -> Result<Option<String>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let result_path = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?
        .join("topology-result.json");
    match fs::read(&result_path) {
        Ok(result) => serde_json::from_slice::<TopologyResult>(&result)
            .map(|result| Some(result.network_sha256))
            .map_err(|error| format!("cannot parse {}: {error}", result_path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", result_path.display())),
    }
}

fn recorded_campaign_actions(plan: &Path) -> Result<Option<Vec<AppliedCampaignAction>>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let result_path = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?
        .join("topology-result.json");
    match fs::read(&result_path) {
        Ok(result) => serde_json::from_slice::<TopologyResult>(&result)
            .map(|result| Some(result.actions))
            .map_err(|error| format!("cannot parse {}: {error}", result_path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", result_path.display())),
    }
}

fn recorded_storage_fingerprints(
    plan: &Path,
    services: &[String],
) -> Result<Option<BTreeMap<String, BTreeMap<String, String>>>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let bundle = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?;
    let mut expected = BTreeMap::new();
    for name in services {
        let result_path = bundle.join("services").join(name).join("result.json");
        let result = fs::read(&result_path)
            .map_err(|error| format!("cannot read {}: {error}", result_path.display()))?;
        let recorded: RecordedServiceResult = serde_json::from_slice(&result)
            .map_err(|error| format!("cannot parse {}: {error}", result_path.display()))?;
        let Some(storage) = recorded.storage_sha256 else {
            return Ok(None);
        };
        expected.insert(name.clone(), storage);
    }
    Ok(Some(expected))
}

fn recorded_network_traffic(
    plan: &Path,
    services: &[String],
) -> Result<Option<BTreeMap<String, BTreeMap<String, NetworkTraffic>>>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let bundle = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?;
    let mut expected = BTreeMap::new();
    for name in services {
        let result_path = bundle.join("services").join(name).join("result.json");
        let result = fs::read(&result_path)
            .map_err(|error| format!("cannot read {}: {error}", result_path.display()))?;
        let recorded: RecordedServiceResult = serde_json::from_slice(&result)
            .map_err(|error| format!("cannot parse {}: {error}", result_path.display()))?;
        let Some(traffic) = recorded.network_traffic else {
            return Ok(None);
        };
        expected.insert(name.clone(), traffic);
    }
    Ok(Some(expected))
}

fn recorded_virtual_times(
    plan: &Path,
    services: &[String],
) -> Result<Option<BTreeMap<String, Option<Vec<u64>>>>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let bundle = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?;
    let mut expected = BTreeMap::new();
    for name in services {
        let result_path = bundle.join("services").join(name).join("result.json");
        let result = fs::read(&result_path)
            .map_err(|error| format!("cannot read {}: {error}", result_path.display()))?;
        let recorded: RecordedServiceResult = serde_json::from_slice(&result)
            .map_err(|error| format!("cannot parse {}: {error}", result_path.display()))?;
        let Some(clock) = recorded.virtual_time_ns else {
            return Ok(None);
        };
        expected.insert(name.clone(), clock);
    }
    Ok(Some(expected))
}

fn network_fingerprint(switches: &BTreeMap<String, SharedSimSwitch>) -> Result<String, String> {
    let ports = switches
        .iter()
        .map(|(name, switch)| {
            Ok((
                name,
                switch
                    .lock()
                    .map_err(|_| "simulated switch lock poisoned".to_owned())?
                    .ports(),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let bytes = serde_json::to_vec(&ports)
        .map_err(|error| format!("cannot encode network topology: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn advance_network_round(
    switches: &BTreeMap<String, SharedSimSwitch>,
    services: &BTreeMap<String, ServiceRuntime>,
) -> Result<(), String> {
    for service in services.values() {
        service.vm.advance_simulated_networks()?;
    }
    for switch in switches.values() {
        switch
            .lock()
            .map_err(|_| "simulated switch lock poisoned".to_owned())?
            .advance_round();
    }
    Ok(())
}

fn recorded_fault_fingerprints(
    plan: &Path,
    services: &[String],
) -> Result<Option<BTreeMap<String, String>>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let bundle = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?;
    let mut expected = BTreeMap::new();
    for name in services {
        let result_path = bundle.join("services").join(name).join("result.json");
        let result = fs::read(&result_path)
            .map_err(|error| format!("cannot read {}: {error}", result_path.display()))?;
        let recorded: RecordedServiceResult = serde_json::from_slice(&result)
            .map_err(|error| format!("cannot parse {}: {error}", result_path.display()))?;
        let fingerprint = recorded
            .faults_sha256
            .map(Ok)
            .unwrap_or_else(|| fault_fingerprint(&recorded.faults))?;
        expected.insert(name.clone(), fingerprint);
    }
    Ok(Some(expected))
}

fn recorded_serial_fingerprints(
    plan: &Path,
    services: &[String],
) -> Result<Option<BTreeMap<String, Vec<String>>>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let bundle = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?;
    let mut expected = BTreeMap::new();
    for name in services {
        let result_path = bundle.join("services").join(name).join("result.json");
        let result = fs::read(&result_path)
            .map_err(|error| format!("cannot read {}: {error}", result_path.display()))?;
        let recorded: RecordedServiceResult = serde_json::from_slice(&result)
            .map_err(|error| format!("cannot parse {}: {error}", result_path.display()))?;
        if !recorded.serial_sha256.is_empty() {
            expected.insert(name.clone(), recorded.serial_sha256);
            continue;
        }
        let logs = if recorded.serial_logs.is_empty() {
            recorded.serial_log.into_iter().collect()
        } else {
            recorded.serial_logs
        };
        if logs.is_empty() {
            return Err(format!(
                "recorded service has no serial logs: {}",
                result_path.display()
            ));
        }
        let paths = logs
            .iter()
            .map(|log| {
                let serial_name = Path::new(log)
                    .file_name()
                    .ok_or_else(|| format!("recorded serial log has no file name: {log}"))?;
                Ok(bundle.join("services").join(name).join(serial_name))
            })
            .collect::<Result<Vec<_>, String>>()?;
        expected.insert(name.clone(), serial_fingerprints(&paths)?);
    }
    Ok(Some(expected))
}

fn serial_fingerprints(serial_logs: &[PathBuf]) -> Result<Vec<String>, String> {
    serial_logs
        .iter()
        .map(|path| {
            let serial = fs::read(path)
                .map_err(|error| format!("cannot read serial log {}: {error}", path.display()))?;
            Ok(format!("{:x}", Sha256::digest(&serial)))
        })
        .collect()
}

fn fault_fingerprint(faults: &[AppliedFault]) -> Result<String, String> {
    let bytes = serde_json::to_vec(faults)
        .map_err(|error| format!("cannot encode applied faults: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn apply_scheduled_faults(
    round: u64,
    name: &str,
    plan: &ServicePlan,
    service_dir: &Path,
    service: &mut ServiceRuntime,
    switches: &mut BTreeMap<String, SharedSimSwitch>,
) -> Result<(), String> {
    if service.paused_until == Some(round) {
        if service.vm.exited().is_none() {
            service.vm.resume()?;
            service.faults.push(AppliedFault {
                round,
                kind: "resume".to_owned(),
                detail: "pause duration elapsed".to_owned(),
            });
        }
        service.paused_until = None;
    }

    while plan
        .faults
        .get(service.next_fault)
        .is_some_and(|fault| fault.at_round == round)
    {
        let fault = &plan.faults[service.next_fault];
        service.next_fault += 1;
        if service.vm.exited().is_some() {
            service.faults.push(AppliedFault {
                round,
                kind: fault_kind_name(&fault.kind).to_owned(),
                detail: "skipped because the service had already exited".to_owned(),
            });
            continue;
        }
        match &fault.kind {
            FaultKind::Pause => {
                let duration = fault.duration_rounds.expect("validated pause fault");
                service.vm.pause()?;
                service.paused_until = Some(round + duration);
                service.faults.push(AppliedFault {
                    round,
                    kind: "pause".to_owned(),
                    detail: format!("paused for {duration} scheduler rounds"),
                });
            }
            FaultKind::Restart => {
                service.record_network_traffic()?;
                service.record_network_trace()?;
                service.vm.stop();
                let serial = service_dir.join(format!("serial-{}.log", service.serial_logs.len()));
                let kernel = service_dir.join("artifacts/kernel");
                let initramfs = service_dir.join("artifacts/initramfs");
                let replacement = build_service(
                    name,
                    service.serial_logs.len(),
                    plan,
                    &kernel,
                    &initramfs,
                    &serial,
                    switches,
                )?;
                replacement.resume()?;
                inject_serial_events(&replacement, &plan.run.events, &serial)?;
                service.vm = replacement;
                service.serial_logs.push(serial);
                service.faults.push(AppliedFault {
                    round,
                    kind: "restart".to_owned(),
                    detail: "cold-restarted from locked service artifacts".to_owned(),
                });
            }
            FaultKind::ClockJump => {
                let nanoseconds = fault.nanoseconds.expect("validated clock jump fault");
                service.vm.jump_virtual_time(nanoseconds)?;
                service.faults.push(AppliedFault {
                    round,
                    kind: "clock_jump".to_owned(),
                    detail: format!("advanced virtual clock by {nanoseconds} ns"),
                });
            }
        }
    }
    Ok(())
}

fn inject_serial_events(
    vm: &ServiceVm,
    events: &[EventPlan],
    serial_log: &Path,
) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if fs::read(serial_log).is_ok_and(|serial| {
            serial
                .windows(b"THES:M:42".len())
                .any(|window| window == b"THES:M:42")
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !fs::read(serial_log).is_ok_and(|serial| {
        serial
            .windows(b"THES:M:42".len())
            .any(|window| window == b"THES:M:42")
    }) {
        return Err(format!(
            "service did not announce serial readiness: {}",
            serial_log.display()
        ));
    }
    for event in events {
        vm.push_serial_input(&decode_hex(&event.data_hex)?)?;
        if let Some(checkpoint) = &event.checkpoint {
            wait_for_serial(
                serial_log,
                checkpoint.as_bytes(),
                "campaign operation checkpoint",
            )?;
        }
    }
    Ok(())
}

fn inject_campaign_events(
    driver_name: &str,
    driver: &mut ServiceRuntime,
    events: &[EventPlan],
    serial_log: &Path,
    topology: &TopologyPlan,
    services: &mut BTreeMap<String, ServiceRuntime>,
    recorded: &mut Vec<AppliedCampaignAction>,
) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    wait_for_serial(serial_log, b"THES:M:42", "serial readiness")?;
    for event in events {
        driver.vm.push_serial_input(&decode_hex(&event.data_hex)?)?;
        if let Some(checkpoint) = &event.checkpoint {
            wait_for_serial(
                serial_log,
                checkpoint.as_bytes(),
                "campaign operation checkpoint",
            )?;
        }
        for action in &event.actions {
            recorded.push(apply_campaign_action(
                action,
                driver_name,
                driver,
                topology,
                services,
            )?);
        }
    }
    Ok(())
}

fn apply_campaign_action(
    action: &CampaignAction,
    driver_name: &str,
    driver: &mut ServiceRuntime,
    topology: &TopologyPlan,
    services: &mut BTreeMap<String, ServiceRuntime>,
) -> Result<AppliedCampaignAction, String> {
    match action.kind {
        CampaignFaultKind::Partition | CampaignFaultKind::Heal => {
            let network = action
                .network
                .as_deref()
                .ok_or_else(|| "campaign action has no network".to_owned())?;
            let partitioned = matches!(action.kind, CampaignFaultKind::Partition);
            let mut endpoints = driver.vm.set_network_partition(network, partitioned)?;
            for service in services.values() {
                endpoints += service.vm.set_network_partition(network, partitioned)?;
            }
            if endpoints == 0 {
                return Err(format!(
                    "campaign action network has no endpoints: {network}"
                ));
            }
            Ok(AppliedCampaignAction {
                operation: action.operation.clone(),
                kind: if partitioned { "partition" } else { "heal" }.to_owned(),
                target: format!("network:{network}"),
                detail: format!(
                    "{} simulated NIC endpoint(s) {} after the operation barrier",
                    endpoints,
                    if partitioned { "partitioned" } else { "healed" }
                ),
            })
        }
        CampaignFaultKind::LinkPartition | CampaignFaultKind::LinkHeal => {
            let network = action
                .network
                .as_deref()
                .ok_or_else(|| "campaign directed link action has no network".to_owned())?;
            let from = action
                .from
                .as_deref()
                .ok_or_else(|| "campaign directed link action has no source".to_owned())?;
            let to = action
                .to
                .as_deref()
                .ok_or_else(|| "campaign directed link action has no destination".to_owned())?;
            let blocked = matches!(action.kind, CampaignFaultKind::LinkPartition);
            let destination = if to == driver_name {
                driver.vm.network_endpoint(network)?
            } else {
                services
                    .get(to)
                    .ok_or_else(|| format!("campaign link destination did not start: {to}"))?
                    .vm
                    .network_endpoint(network)?
            }
            .ok_or_else(|| {
                format!("campaign link destination is not on network: {to}/{network}")
            })?;
            if from == driver_name {
                driver.vm.set_network_link(network, &destination, blocked)?;
            } else {
                services
                    .get(from)
                    .ok_or_else(|| format!("campaign link source did not start: {from}"))?
                    .vm
                    .set_network_link(network, &destination, blocked)?;
            }
            Ok(AppliedCampaignAction {
                operation: action.operation.clone(),
                kind: if blocked {
                    "link_partition"
                } else {
                    "link_heal"
                }
                .to_owned(),
                target: format!("network:{network}/{from}->{to}"),
                detail: format!(
                    "directed link {} after the operation barrier",
                    if blocked { "partitioned" } else { "healed" }
                ),
            })
        }
        CampaignFaultKind::NetworkFault | CampaignFaultKind::NetworkRecover => {
            let network = action
                .network
                .as_deref()
                .ok_or_else(|| "campaign network action has no network".to_owned())?;
            let recover = matches!(action.kind, CampaignFaultKind::NetworkRecover);
            let driver_baseline = &topology
                .services
                .get(driver_name)
                .ok_or_else(|| format!("campaign driver disappeared: {driver_name}"))?
                .run
                .network;
            let mut endpoints = driver.vm.set_network_conditions(
                network,
                driver_baseline,
                (!recover).then_some(action),
            )?;
            for (service_name, service) in services.iter_mut() {
                let baseline = &topology
                    .services
                    .get(service_name)
                    .ok_or_else(|| format!("campaign service disappeared: {service_name}"))?
                    .run
                    .network;
                endpoints += service.vm.set_network_conditions(
                    network,
                    baseline,
                    (!recover).then_some(action),
                )?;
            }
            if endpoints == 0 {
                return Err(format!(
                    "campaign network action network has no endpoints: {network}"
                ));
            }
            let detail = if recover {
                "restored declared packet conditions".to_owned()
            } else {
                let mut conditions = Vec::new();
                for (name, value) in [
                    ("drop_ppm", action.drop_ppm.map(|value| value.to_string())),
                    (
                        "duplicate_ppm",
                        action.duplicate_ppm.map(|value| value.to_string()),
                    ),
                    (
                        "corrupt_ppm",
                        action.corrupt_ppm.map(|value| value.to_string()),
                    ),
                    (
                        "latency_rounds",
                        action.latency_rounds.map(|value| value.to_string()),
                    ),
                    (
                        "jitter_rounds",
                        action.jitter_rounds.map(|value| value.to_string()),
                    ),
                    (
                        "tx_bytes_per_round",
                        action.tx_bytes_per_round.map(|value| value.to_string()),
                    ),
                    ("mtu_bytes", action.mtu_bytes.map(|value| value.to_string())),
                    (
                        "tx_queue_frames",
                        action.tx_queue_frames.map(|value| value.to_string()),
                    ),
                    (
                        "rx_queue_frames",
                        action.rx_queue_frames.map(|value| value.to_string()),
                    ),
                ] {
                    if let Some(value) = value {
                        conditions.push(format!("{name}={value}"));
                    }
                }
                conditions.join(", ")
            };
            Ok(AppliedCampaignAction {
                operation: action.operation.clone(),
                kind: if recover {
                    "network_recover"
                } else {
                    "network_fault"
                }
                .to_owned(),
                target: format!("network:{network}"),
                detail: format!("{endpoints} simulated NIC endpoint(s): {detail}"),
            })
        }
        CampaignFaultKind::StorageFault | CampaignFaultKind::StorageRecover => {
            let service_name = action
                .service
                .as_deref()
                .ok_or_else(|| "campaign storage action has no service".to_owned())?;
            let drive = action
                .drive
                .as_deref()
                .ok_or_else(|| "campaign storage action has no drive".to_owned())?;
            let recover = matches!(action.kind, CampaignFaultKind::StorageRecover);
            let storage = &topology
                .services
                .get(service_name)
                .ok_or_else(|| format!("campaign storage service disappeared: {service_name}"))?
                .run
                .storage;
            let baseline = storage
                .iter()
                .find(|item| item.id == drive)
                .ok_or_else(|| format!("campaign storage drive disappeared: {drive}"))?;
            let error_ppm = if recover {
                baseline.error_ppm
            } else {
                action.error_ppm.unwrap_or(0)
            };
            let latency_rounds = if recover {
                baseline.latency_rounds
            } else {
                action.latency_rounds.unwrap_or(0)
            };
            let torn_write_bytes = if recover {
                baseline.torn_write_bytes
            } else {
                action.torn_write_bytes
            };
            let corrupt_read_xor = if recover {
                baseline.corrupt_read_xor
            } else {
                action.corrupt_read_xor
            };
            if service_name == driver_name {
                driver.vm.set_storage_fault(
                    storage,
                    drive,
                    error_ppm,
                    latency_rounds,
                    torn_write_bytes,
                    corrupt_read_xor,
                )?;
            } else {
                services
                    .get_mut(service_name)
                    .ok_or_else(|| {
                        format!("campaign storage service did not start: {service_name}")
                    })?
                    .vm
                    .set_storage_fault(
                        storage,
                        drive,
                        error_ppm,
                        latency_rounds,
                        torn_write_bytes,
                        corrupt_read_xor,
                    )?;
            }
            Ok(AppliedCampaignAction {
                operation: action.operation.clone(),
                kind: if recover {
                    "storage_recover"
                } else {
                    "storage_fault"
                }
                .to_owned(),
                target: format!("service:{service_name}/drive:{drive}"),
                detail: format!(
                    "error_ppm={error_ppm}, latency_rounds={latency_rounds}, torn_write_bytes={torn_write_bytes:?}, corrupt_read_xor={corrupt_read_xor:?}"
                ),
            })
        }
        CampaignFaultKind::Pause | CampaignFaultKind::Restart | CampaignFaultKind::ClockJump => {
            Err("campaign lifecycle fault cannot be applied at an operation barrier".to_owned())
        }
    }
}

fn wait_for_serial(serial_log: &Path, needle: &[u8], purpose: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if fs::read(serial_log)
            .is_ok_and(|serial| serial.windows(needle.len()).any(|window| window == needle))
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(format!(
        "service did not announce {purpose}: {}",
        serial_log.display()
    ))
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 {
        return Err("serial event has incomplete hex bytes".to_owned());
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16).map_err(|error| error.to_string())
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn fault_kind_name(kind: &FaultKind) -> &'static str {
    match kind {
        FaultKind::Pause => "pause",
        FaultKind::Restart => "restart",
        FaultKind::ClockJump => "clock_jump",
    }
}

fn evaluate_checks(checks: &[CheckPlan], serial_logs: &[PathBuf]) -> Vec<CheckResult> {
    checks
        .iter()
        .map(|check| {
            let needle = match check.kind {
                CheckKind::MarkerSeen | CheckKind::MarkerNotSeen => {
                    format!("THES:M:{}", check.value).into_bytes()
                }
                _ => check.value.as_bytes().to_vec(),
            };
            let contains = serial_logs.iter().any(|path| {
                fs::read(path)
                    .unwrap_or_default()
                    .windows(needle.len())
                    .any(|window| window == needle)
            });
            let passed = match check.kind {
                CheckKind::SerialContains | CheckKind::MarkerSeen => contains,
                CheckKind::SerialNotContains | CheckKind::MarkerNotSeen => !contains,
            };
            CheckResult {
                name: check.name.clone(),
                status: if passed { "passed" } else { "failed" },
                detail: if passed {
                    "serial log satisfied check".to_owned()
                } else {
                    "serial log did not satisfy check".to_owned()
                },
            }
        })
        .collect()
}

fn build_service(
    name: &str,
    instance: usize,
    service: &ServicePlan,
    kernel: &Path,
    initramfs: &Path,
    serial: &Path,
    switches: &mut BTreeMap<String, SharedSimSwitch>,
) -> Result<ServiceVm, String> {
    let mut resources = VmResources::default();
    resources
        .build_boot_source(BootSourceConfig {
            kernel_image_path: kernel.display().to_string(),
            initrd_path: Some(initramfs.display().to_string()),
            boot_args: Some("console=ttyS0 reboot=k panic=-1".to_owned()),
        })
        .map_err(|error| error.to_string())?;
    resources
        .update_machine_config(&MachineConfigUpdate {
            vcpu_count: Some(service.run.run.vcpu_count),
            mem_size_mib: Some(service.run.run.mem_size_mib as usize),
            virtual_time: service
                .run
                .run
                .virtual_time
                .as_ref()
                .map(|time| VirtualTimeConfig {
                    tick_ns: time.tick_ns,
                    exits_per_tick: time.exits_per_tick as u64,
                }),
            ..Default::default()
        })
        .map_err(|error| error.to_string())?;
    resources
        .entropy
        .insert(EntropyDeviceConfig {
            rate_limiter: None,
            seed: Some(service.run.run.seed),
            script: None,
        })
        .map_err(|error| error.to_string())?;
    resources.serial_out_path = Some(serial.to_path_buf());
    let mut simulated_storage = Vec::new();
    let mut simulated_networks = Vec::new();
    for storage in &service.run.storage {
        let block = Arc::new(Mutex::new(
            Block::new_simulated(SimulatedBlockConfig {
                drive_id: storage.id.clone(),
                size_mib: storage.size_mib,
                seed: storage.seed,
                error_ppm: storage.error_ppm,
                latency_rounds: storage.latency_rounds,
                torn_write_bytes: storage.torn_write_bytes,
                corrupt_read_xor: storage.corrupt_read_xor,
            })
            .map_err(|error| {
                format!(
                    "service {name}: cannot create storage {:?}: {error}",
                    storage.id
                )
            })?,
        ));
        resources.block.add_virtio_device(block.clone());
        simulated_storage.push(block);
    }
    for network in &service.networks {
        let switch = switches
            .get(network)
            .ok_or_else(|| format!("service {name}: unknown network {network}"))?
            .clone();
        let net = Arc::new(Mutex::new(
            Net::new_with_sim_switch(
                format!("net-{network}"),
                SimNetConfig {
                    seed: service.run.run.seed,
                    loopback: service.run.network.loopback,
                    drop_ppm: service.run.network.drop_ppm,
                    duplicate_ppm: service.run.network.duplicate_ppm,
                    corrupt_ppm: service.run.network.corrupt_ppm,
                    partitioned: service.run.network.partitioned,
                    latency_rounds: service.run.network.latency_rounds,
                    jitter_rounds: service.run.network.jitter_rounds,
                    tx_bytes_per_round: service.run.network.tx_bytes_per_round,
                    mtu_bytes: service.run.network.mtu_bytes,
                    tx_queue_frames: service.run.network.tx_queue_frames,
                    rx_queue_frames: service.run.network.rx_queue_frames,
                },
                switch,
                format!("{network}/{name}-{instance}"),
                None,
                RateLimiter::default(),
                RateLimiter::default(),
                None,
            )
            .map_err(|error| error.to_string())?,
        ));
        resources.net_builder.add_device(net.clone());
        simulated_networks.push((network.clone(), net));
    }
    let mut event_manager = EventManager::new().map_err(|error| error.to_string())?;
    let vmm = build_microvm_for_boot(
        &InstanceInfo::default(),
        &resources,
        &mut event_manager,
        &get_empty_filters(),
    )
    .map_err(|error| error.to_string())?;
    Ok(ServiceVm {
        vmm,
        event_manager,
        storage: simulated_storage,
        networks: simulated_networks,
    })
}

fn lock_artifact(service_dir: &Path, name: &str, artifact: &Artifact) -> Result<PathBuf, String> {
    let source = Path::new(&artifact.path);
    let bytes =
        fs::read(source).map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if digest != artifact.sha256 {
        return Err(format!("artifact digest changed: {}", source.display()));
    }
    let target = service_dir.join("artifacts").join(name);
    fs::copy(source, &target).map_err(|error| error.to_string())?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn campaign_fault(kind: CampaignFaultKind) -> CampaignFault {
        CampaignFault {
            kind,
            service: None,
            network: Some("backplane".to_owned()),
            from: None,
            to: None,
            drive: None,
            after: Some("write".to_owned()),
            at_round: None,
            duration_rounds: None,
            nanoseconds: None,
            error_ppm: None,
            latency_rounds: None,
            torn_write_bytes: None,
            corrupt_read_xor: None,
            drop_ppm: None,
            duplicate_ppm: None,
            corrupt_ppm: None,
            jitter_rounds: None,
            tx_bytes_per_round: None,
            mtu_bytes: None,
            tx_queue_frames: None,
            rx_queue_frames: None,
        }
    }

    #[test]
    fn campaign_fault_combinations_keep_declaration_order() {
        let faults = vec![
            campaign_fault(CampaignFaultKind::Partition),
            campaign_fault(CampaignFaultKind::Heal),
            campaign_fault(CampaignFaultKind::LinkPartition),
        ];
        assert_eq!(
            campaign_fault_combinations(&faults, &[0, 1, 2], 2),
            vec![
                vec![0],
                vec![0, 1],
                vec![0, 2],
                vec![1],
                vec![1, 2],
                vec![2]
            ]
        );
    }

    #[test]
    fn campaign_network_action_keeps_each_selected_condition() {
        let mut fault = campaign_fault(CampaignFaultKind::NetworkFault);
        fault.drop_ppm = Some(1_000_000);
        fault.latency_rounds = Some(3);
        fault.tx_queue_frames = Some(2);

        let action = campaign_action(&fault).unwrap();
        assert!(matches!(action.kind, CampaignFaultKind::NetworkFault));
        assert_eq!(action.drop_ppm, Some(1_000_000));
        assert_eq!(action.latency_rounds, Some(3));
        assert_eq!(action.tx_queue_frames, Some(2));
    }

    #[test]
    fn campaign_storage_recovery_keeps_the_drive_target() {
        let mut fault = campaign_fault(CampaignFaultKind::StorageRecover);
        fault.service = Some("replica".to_owned());
        fault.network = None;
        fault.drive = Some("data".to_owned());
        fault.after = Some("retry".to_owned());

        let action = campaign_action(&fault).unwrap();
        assert!(matches!(action.kind, CampaignFaultKind::StorageRecover));
        assert_eq!(action.service.as_deref(), Some("replica"));
        assert_eq!(action.drive.as_deref(), Some("data"));
        assert_eq!(action.operation, "retry");
    }

    #[test]
    fn recorded_serial_fingerprints_use_bundle_local_logs() {
        let bundle = std::env::temp_dir().join(format!(
            "theseus-topology-serial-fingerprint-{}",
            std::process::id()
        ));
        let service = bundle.join("services/api");
        fs::create_dir_all(&service).unwrap();
        fs::write(bundle.join("replay-plan.json"), "{}").unwrap();
        fs::write(service.join("serial.log"), b"ready\n").unwrap();
        fs::write(
            service.join("result.json"),
            r#"{"serial_logs":["/previous/location/serial.log"]}"#,
        )
        .unwrap();

        let fingerprints =
            recorded_serial_fingerprints(&bundle.join("replay-plan.json"), &["api".to_owned()])
                .unwrap()
                .unwrap();

        assert_eq!(
            fingerprints["api"],
            vec![format!("{:x}", Sha256::digest(b"ready\n"))]
        );
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn recorded_fault_fingerprints_use_recorded_digest() {
        let bundle = std::env::temp_dir().join(format!(
            "theseus-topology-fault-fingerprint-{}",
            std::process::id()
        ));
        let service = bundle.join("services/api");
        fs::create_dir_all(&service).unwrap();
        fs::write(bundle.join("replay-plan.json"), "{}").unwrap();
        fs::write(
            service.join("result.json"),
            r#"{"faults_sha256":"recorded-fault-digest"}"#,
        )
        .unwrap();

        let fingerprints =
            recorded_fault_fingerprints(&bundle.join("replay-plan.json"), &["api".to_owned()])
                .unwrap()
                .unwrap();

        assert_eq!(fingerprints["api"], "recorded-fault-digest");
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn recorded_network_fingerprint_uses_topology_result() {
        let bundle = std::env::temp_dir().join(format!(
            "theseus-topology-network-fingerprint-{}",
            std::process::id()
        ));
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("replay-plan.json"), "{}").unwrap();
        fs::write(
            bundle.join("topology-result.json"),
            r#"{"network_sha256":"recorded-network-digest"}"#,
        )
        .unwrap();

        assert_eq!(
            recorded_network_fingerprint(&bundle.join("replay-plan.json")).unwrap(),
            Some("recorded-network-digest".to_owned())
        );
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn recorded_campaign_actions_use_topology_result() {
        let bundle = std::env::temp_dir().join(format!(
            "theseus-topology-action-fingerprint-{}",
            std::process::id()
        ));
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("replay-plan.json"), "{}").unwrap();
        fs::write(
            bundle.join("topology-result.json"),
            r#"{"network_sha256":"network","actions":[{"operation":"write","kind":"partition","target":"network:backplane","detail":"two endpoints"}]}"#,
        )
        .unwrap();

        assert_eq!(
            recorded_campaign_actions(&bundle.join("replay-plan.json")).unwrap(),
            Some(vec![AppliedCampaignAction {
                operation: "write".to_owned(),
                kind: "partition".to_owned(),
                target: "network:backplane".to_owned(),
                detail: "two endpoints".to_owned(),
            }])
        );
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn recorded_storage_fingerprints_use_recorded_drive_digests() {
        let bundle = std::env::temp_dir().join(format!(
            "theseus-topology-storage-fingerprint-{}",
            std::process::id()
        ));
        let service = bundle.join("services/api");
        fs::create_dir_all(&service).unwrap();
        fs::write(bundle.join("replay-plan.json"), "{}").unwrap();
        fs::write(
            service.join("result.json"),
            r#"{"storage_sha256":{"data":"recorded-storage-digest"}}"#,
        )
        .unwrap();

        let fingerprints =
            recorded_storage_fingerprints(&bundle.join("replay-plan.json"), &["api".to_owned()])
                .unwrap()
                .unwrap();

        assert_eq!(fingerprints["api"]["data"], "recorded-storage-digest");
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn recorded_network_traffic_uses_recorded_frame_counters() {
        let bundle = std::env::temp_dir().join(format!(
            "theseus-topology-network-traffic-{}",
            std::process::id()
        ));
        let service = bundle.join("services/api");
        fs::create_dir_all(&service).unwrap();
        fs::write(bundle.join("replay-plan.json"), "{}").unwrap();
        fs::write(
            service.join("result.json"),
            r#"{"network_traffic":{"backplane":{"tx_frames":3,"rx_frames":2,"dropped":1}}}"#,
        )
        .unwrap();

        let traffic =
            recorded_network_traffic(&bundle.join("replay-plan.json"), &["api".to_owned()])
                .unwrap()
                .unwrap();

        assert_eq!(
            traffic["api"]["backplane"],
            NetworkTraffic {
                tx_frames: 3,
                rx_frames: 2,
                dropped: 1,
                duplicated: 0,
                corrupted: 0,
                tx_sha256: None,
                rx_sha256: None,
            }
        );
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn network_traffic_payload_fingerprints_reject_changed_frames() {
        let expected = BTreeMap::from([(
            "backplane".to_owned(),
            NetworkTraffic {
                tx_frames: 1,
                rx_frames: 1,
                dropped: 0,
                duplicated: 0,
                corrupted: 0,
                tx_sha256: Some("original-tx".to_owned()),
                rx_sha256: Some("original-rx".to_owned()),
            },
        )]);
        let changed = BTreeMap::from([(
            "backplane".to_owned(),
            NetworkTraffic {
                tx_frames: 1,
                rx_frames: 1,
                dropped: 0,
                duplicated: 0,
                corrupted: 0,
                tx_sha256: Some("changed-tx".to_owned()),
                rx_sha256: Some("changed-rx".to_owned()),
            },
        )]);
        assert!(!traffic_matches(&expected, &changed));

        let mut extra_corruption = expected.clone();
        extra_corruption
            .get_mut("backplane")
            .expect("fixture includes backplane")
            .corrupted = 1;
        assert!(!traffic_matches(&expected, &extra_corruption));

        let legacy = BTreeMap::from([(
            "backplane".to_owned(),
            NetworkTraffic {
                tx_sha256: None,
                rx_sha256: None,
                ..expected["backplane"].clone()
            },
        )]);
        assert!(traffic_matches(&legacy, &changed));
    }

    #[test]
    fn recorded_virtual_times_use_recorded_vcpu_clocks() {
        let bundle = std::env::temp_dir().join(format!(
            "theseus-topology-virtual-time-{}",
            std::process::id()
        ));
        let service = bundle.join("services/api");
        fs::create_dir_all(&service).unwrap();
        fs::write(bundle.join("replay-plan.json"), "{}").unwrap();
        fs::write(service.join("result.json"), r#"{"virtual_time_ns":[1000]}"#).unwrap();

        let clocks = recorded_virtual_times(&bundle.join("replay-plan.json"), &["api".to_owned()])
            .unwrap()
            .unwrap();

        assert_eq!(clocks["api"], Some(vec![1000]));
        fs::remove_dir_all(bundle).unwrap();
    }
}
