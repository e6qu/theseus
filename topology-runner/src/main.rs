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
use theseus_engine::simnet::{SharedSimSwitch, SimSwitch, SimSwitchState};
use vmm::builder::build_microvm_for_boot;
use vmm::devices::virtio::block::device::Block;
use vmm::devices::virtio::block::virtio::device::SimulatedBlockConfig;
use vmm::devices::virtio::net::{
    Net, SimNetConfig, SimNetDropReason, SimNetFrameDirection, SimNetPacketSelector, SimNetState,
};
use vmm::persist::{create_snapshot, restore_from_snapshot, VmInfo};
use vmm::rate_limiter::RateLimiter;
use vmm::resources::VmResources;
use vmm::seccomp::get_empty_filters;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::entropy::EntropyDeviceConfig;
use vmm::vmm_config::instance_info::InstanceInfo;
use vmm::vmm_config::machine_config::{MachineConfigUpdate, VirtualTimeConfig};
use vmm::vmm_config::snapshot::{
    CreateSnapshotParams, LoadSnapshotParams, MemBackendConfig, MemBackendType,
    SnapshotLoadHugePageConfig, SnapshotType,
};
use vmm::{EventManager, FcExitCode, Vmm};

const USAGE: &str =
    "Usage: theseus-topology --plan topology-plan.json --output replay-dir [--minimize]";
const MAX_CAMPAIGN_CANDIDATES: usize = 4_096;

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
    ethertype: Option<u16>,
    #[serde(default)]
    ip_protocol: Option<u8>,
    #[serde(default)]
    source_port: Option<u16>,
    #[serde(default)]
    destination_port: Option<u16>,
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
    PacketFault,
    PacketRecover,
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
    checkpoint_nodes: usize,
    checkpoint_reuses: usize,
    generated_candidates: usize,
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
    selection: String,
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
    ethertype: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip_protocol: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_port: Option<u16>,
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
    storage: Vec<String>,
    networks: Vec<(String, String)>,
    network_endpoints: BTreeMap<String, String>,
}

#[derive(Clone)]
struct ServiceVmCheckpoint {
    snapshot_path: PathBuf,
    memory_path: PathBuf,
    networks: BTreeMap<String, SimNetState>,
}

#[derive(Clone)]
struct ServiceSchedulerCheckpoint {
    serial_contents: Vec<Vec<u8>>,
    next_fault: usize,
    paused_until: Option<u64>,
    faults: Vec<AppliedFault>,
    network_traffic: BTreeMap<String, NetworkTraffic>,
    network_trace: BTreeMap<String, Vec<NetworkFrame>>,
}

#[derive(Clone)]
struct CampaignCheckpoint {
    switches: BTreeMap<String, SimSwitchState>,
    services: BTreeMap<String, ServiceVmCheckpoint>,
    scheduler: BTreeMap<String, ServiceSchedulerCheckpoint>,
    round: u64,
}

/// One materialized node in a campaign operation-prefix tree. The VMM state
/// is deliberately not serialized into the result JSON: its immutable
/// Firecracker snapshots already live under `checkpoints/` and are consumed
/// only by this invocation. The locked replay bundle remains a normal,
/// self-contained event plan.
#[derive(Clone)]
struct CampaignPrefixCheckpoint {
    checkpoint: CampaignCheckpoint,
    actions: Vec<AppliedCampaignAction>,
}

/// Restore a checkpoint for each distinct operation/action prefix once, then
/// fork every leaf from its nearest materialized ancestor. This is a real tree
/// rather than a cache keyed only by operation names: the key includes the
/// exact serial input and barrier actions, so a faulted prefix never leaks into
/// an ordinary sibling.
struct CampaignCheckpointTree {
    root: CampaignCheckpoint,
    prefixes: BTreeMap<String, CampaignPrefixCheckpoint>,
    reuses: usize,
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
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        for (_, id) in &self.networks {
            vmm.with_simulated_network(id, |net| net.advance_simulated_round())
                .ok_or_else(|| format!("network device disappeared: {id}"))?;
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
            .map(|(plan, id)| {
                let vmm = self.vmm.lock().expect("VMM lock poisoned");
                let bytes = vmm
                    .with_simulated_block(id, |block| {
                        block.simulated_bytes().map(ToOwned::to_owned)
                    })
                    .flatten()
                    .ok_or_else(|| format!("storage is not simulated: {}", plan.id))?;
                Ok((plan.id.clone(), format!("{:x}", Sha256::digest(bytes))))
            })
            .collect()
    }

    fn set_network_partition(&self, network: &str, partitioned: bool) -> Result<usize, String> {
        let mut changed = 0;
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        for (name, id) in &self.networks {
            if name != network {
                continue;
            }
            if !vmm
                .with_simulated_network(id, |net| net.set_simulated_partitioned(partitioned))
                .unwrap_or(false)
            {
                return Err(format!("network is not simulated: {network}"));
            }
            changed += 1;
        }
        Ok(changed)
    }

    fn save_network_states(&self) -> Result<BTreeMap<String, SimNetState>, String> {
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        self.networks
            .iter()
            .map(|(name, id)| {
                vmm.simulated_network_state(id)
                    .map(|state| (name.clone(), state))
                    .ok_or_else(|| format!("network is not simulated: {name}"))
            })
            .collect()
    }

    fn restore_network_states(&self, states: BTreeMap<String, SimNetState>) -> Result<(), String> {
        if states.len() != self.networks.len() {
            return Err("checkpoint network set does not match service".to_owned());
        }
        let mut vmm = self.vmm.lock().expect("VMM lock poisoned");
        for (name, id) in &self.networks {
            let state = states
                .get(name)
                .ok_or_else(|| format!("checkpoint is missing network: {name}"))?
                .clone();
            if !vmm.restore_simulated_network(id, state) {
                return Err(format!("network is not simulated: {name}"));
            }
        }
        Ok(())
    }

    fn set_network_conditions(
        &self,
        network: &str,
        baseline: &NetworkConfig,
        action: Option<&CampaignAction>,
    ) -> Result<usize, String> {
        let mut changed = 0;
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        for (name, id) in &self.networks {
            if name != network {
                continue;
            }
            let current = vmm
                .with_simulated_network(id, |net| net.sim_config())
                .flatten()
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
            if !vmm
                .with_simulated_network(id, |net| net.set_simulated_conditions(conditions))
                .unwrap_or(false)
            {
                return Err(format!("network is not simulated: {network}"));
            }
            changed += 1;
        }
        Ok(changed)
    }

    fn set_network_packet_drop_rule(
        &self,
        network: &str,
        selector: SimNetPacketSelector,
        drop_ppm: Option<u32>,
    ) -> Result<usize, String> {
        let mut changed = 0;
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        for (name, id) in &self.networks {
            if name != network {
                continue;
            }
            if !vmm
                .with_simulated_network(id, |net| {
                    net.set_simulated_packet_drop_rule(selector, drop_ppm)
                })
                .unwrap_or(false)
            {
                return Err(format!("network is not simulated: {network}"));
            }
            changed += 1;
        }
        Ok(changed)
    }

    fn set_network_link_packet_drop_rule(
        &self,
        network: &str,
        destination: &str,
        selector: SimNetPacketSelector,
        drop_ppm: Option<u32>,
    ) -> Result<(), String> {
        let net = self
            .networks
            .iter()
            .find(|(name, _)| name == network)
            .map(|(_, id)| id)
            .ok_or_else(|| format!("campaign packet source is not on network: {network}"))?;
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        if !vmm
            .with_simulated_network(net, |net| {
                net.set_simulated_link_packet_drop_rule(destination, selector, drop_ppm)
            })
            .unwrap_or(false)
        {
            return Err(format!(
                "network is not a simulated topology link: {network}"
            ));
        }
        Ok(())
    }

    fn network_endpoint(&self, network: &str) -> Result<Option<String>, String> {
        self.networks
            .iter()
            .find(|(name, _)| name == network)
            .map(|(_, id)| {
                self.vmm
                    .lock()
                    .expect("VMM lock poisoned")
                    .with_simulated_network(id, |net| net.simulated_endpoint())
                    .flatten()
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
        let (_, id) = self
            .networks
            .iter()
            .find(|(name, _)| name == network)
            .ok_or_else(|| format!("service is not on network: {network}"))?;
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        if !vmm
            .with_simulated_network(id, |net| {
                net.set_simulated_link_blocked(destination, blocked)
            })
            .unwrap_or(false)
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
        let id = self
            .storage
            .get(index)
            .ok_or_else(|| format!("simulated storage disappeared: {drive}"))?;
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        if !vmm
            .with_simulated_block(id, |block| {
                block.set_simulated_faults(
                    error_ppm,
                    latency_rounds,
                    torn_write_bytes,
                    corrupt_read_xor,
                )
            })
            .unwrap_or(false)
        {
            return Err(format!("storage is not simulated: {drive}"));
        }
        Ok(())
    }

    fn network_traffic(&self) -> Result<BTreeMap<String, NetworkTraffic>, String> {
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        self.networks
            .iter()
            .map(|(name, id)| {
                let stats = vmm
                    .with_simulated_network(id, |net| net.simulated_stats())
                    .flatten()
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
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        self.networks
            .iter()
            .map(|(name, id)| {
                let trace = vmm
                    .with_simulated_network(id, |net| net.simulated_trace())
                    .flatten()
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
                                    SimNetDropReason::PacketRule => "packet_rule",
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

    fn snapshot(&mut self, directory: &Path) -> Result<ServiceVmCheckpoint, String> {
        fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        let networks = self.save_network_states()?;
        let snapshot_path = directory.join("state.snap");
        let memory_path = directory.join("memory.snap");
        let mut vmm = self.vmm.lock().expect("VMM lock poisoned");
        let vm_info = VmInfo::from(&*vmm);
        create_snapshot(
            &mut vmm,
            &vm_info,
            &CreateSnapshotParams {
                snapshot_type: SnapshotType::Full,
                snapshot_path: snapshot_path.clone(),
                mem_file_path: memory_path.clone(),
                sync_snapshot_files: true,
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(ServiceVmCheckpoint {
            snapshot_path,
            memory_path,
            networks,
        })
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

fn capture_campaign_checkpoint(
    directory: &Path,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    round: u64,
) -> Result<CampaignCheckpoint, String> {
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    // Stop every vCPU before capturing any VM or topology-owned state. This
    // is a barrier: no NIC queue, serial byte, or scheduler action can land
    // in only one side of the checkpoint.
    for service in services.values() {
        service.vm.pause()?;
    }
    let result = (|| {
        let mut snapshots = BTreeMap::new();
        let mut scheduler = BTreeMap::new();
        for (name, service) in services.iter_mut() {
            let serial_contents = service
                .serial_logs
                .iter()
                .map(|path| {
                    fs::read(path)
                        .map_err(|error| format!("cannot checkpoint {}: {error}", path.display()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            snapshots.insert(
                name.clone(),
                service
                    .vm
                    .snapshot(&directory.join("services").join(name))?,
            );
            scheduler.insert(
                name.clone(),
                ServiceSchedulerCheckpoint {
                    serial_contents,
                    next_fault: service.next_fault,
                    paused_until: service.paused_until,
                    faults: service.faults.clone(),
                    network_traffic: service.network_traffic.clone(),
                    network_trace: service.network_trace.clone(),
                },
            );
        }
        let switches = switches
            .iter()
            .map(|(name, switch)| {
                switch
                    .lock()
                    .map_err(|_| "simulated switch lock poisoned".to_owned())
                    .map(|switch| (name.clone(), switch.save_state()))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        Ok(CampaignCheckpoint {
            switches,
            services: snapshots,
            scheduler,
            round,
        })
    })();
    for service in services.values() {
        service.vm.stop();
    }
    result
}

fn restore_campaign_serial_logs(
    directory: &Path,
    checkpoint: &ServiceSchedulerCheckpoint,
) -> Result<Vec<PathBuf>, String> {
    checkpoint
        .serial_contents
        .iter()
        .enumerate()
        .map(|(index, contents)| {
            let path = if index == 0 {
                directory.join("serial.log")
            } else {
                directory.join(format!("serial-{index}.log"))
            };
            fs::write(&path, contents).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .collect()
}

impl CampaignCheckpointTree {
    fn new(root: CampaignCheckpoint) -> Self {
        Self {
            root,
            prefixes: BTreeMap::new(),
            reuses: 0,
        }
    }

    fn checkpoint_for_schedule(
        &mut self,
        topology: &TopologyPlan,
        campaign: &CampaignPlan,
        schedule: &CampaignSchedule,
        directory: &Path,
    ) -> Result<CampaignPrefixCheckpoint, String> {
        let events = campaign_schedule_events(campaign, schedule)?;
        let mut prefix = Vec::new();
        let mut parent = CampaignPrefixCheckpoint {
            checkpoint: self.root.clone(),
            actions: Vec::new(),
        };
        for event in events {
            prefix.push(event);
            let key = campaign_prefix_key(&prefix)?;
            if let Some(existing) = self.prefixes.get(&key) {
                self.reuses += 1;
                parent = existing.clone();
                continue;
            }
            let (checkpoint, applied) = checkpoint_campaign_operation(
                topology,
                &campaign.driver,
                &parent.checkpoint,
                &prefix[prefix.len() - 1],
                &directory.join("checkpoints").join(&key),
            )?;
            let mut actions = parent.actions.clone();
            actions.extend(applied);
            parent = CampaignPrefixCheckpoint {
                checkpoint,
                actions,
            };
            self.prefixes.insert(key, parent.clone());
        }
        Ok(parent)
    }

    fn nodes(&self) -> usize {
        // Include the boot/readiness root, which is also a reusable snapshot.
        self.prefixes.len() + 1
    }
}

fn campaign_prefix_key(events: &[EventPlan]) -> Result<String, String> {
    let encoded = serde_json::to_vec(events)
        .map_err(|error| format!("cannot encode campaign checkpoint prefix: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

/// Resume a parent topology snapshot just long enough to execute one driver
/// operation. Its UART checkpoint is the fork barrier. We then stop every
/// vCPU and capture the complete resulting topology, so sibling operations
/// begin from byte-identical VM, disk, network, serial, and scheduler state.
fn checkpoint_campaign_operation(
    topology: &TopologyPlan,
    driver_name: &str,
    parent: &CampaignCheckpoint,
    event: &EventPlan,
    directory: &Path,
) -> Result<(CampaignCheckpoint, Vec<AppliedCampaignAction>), String> {
    let switches: BTreeMap<String, SharedSimSwitch> = topology
        .networks
        .keys()
        .map(|name| (name.clone(), Arc::new(Mutex::new(SimSwitch::new()))))
        .collect();
    let names = topology.services.keys().cloned().collect::<Vec<_>>();
    let mut services = BTreeMap::new();
    for name in &names {
        let service = &topology.services[name];
        let service_dir = directory.join("services").join(name);
        fs::create_dir_all(&service_dir).map_err(|error| error.to_string())?;
        let scheduler = parent
            .scheduler
            .get(name)
            .ok_or_else(|| format!("checkpoint is missing scheduler state for {name}"))?;
        let serial_logs = restore_campaign_serial_logs(&service_dir, scheduler)?;
        let serial = serial_logs
            .first()
            .ok_or_else(|| format!("checkpoint has no serial log for {name}"))?;
        let vm = restore_service(
            name,
            0,
            service,
            Path::new(&service.run.guest.kernel.path),
            Path::new(&service.run.guest.initramfs.path),
            serial,
            &switches,
            parent
                .services
                .get(name)
                .ok_or_else(|| format!("checkpoint is missing VM state for {name}"))?,
        )?;
        services.insert(
            name.clone(),
            ServiceRuntime {
                vm,
                serial_logs,
                next_fault: scheduler.next_fault,
                paused_until: scheduler.paused_until,
                faults: scheduler.faults.clone(),
                network_traffic: scheduler.network_traffic.clone(),
                network_trace: scheduler.network_trace.clone(),
            },
        );
    }
    for (name, state) in &parent.switches {
        switches
            .get(name)
            .ok_or_else(|| format!("checkpoint switch disappeared: {name}"))?
            .lock()
            .map_err(|_| "simulated switch lock poisoned".to_owned())?
            .restore_state(state.clone())
            .map_err(|error| error.to_string())?;
    }
    for name in &names {
        services[name].vm.resume()?;
    }
    let mut driver = services
        .remove(driver_name)
        .ok_or_else(|| format!("campaign driver disappeared: {driver_name}"))?;
    let serial = driver.serial_logs[0].clone();
    let mut applied = Vec::new();
    let injection = inject_campaign_events(
        driver_name,
        &mut driver,
        std::slice::from_ref(event),
        &serial,
        topology,
        &mut services,
        &mut applied,
    );
    services.insert(driver_name.to_owned(), driver);
    injection?;
    let checkpoint =
        capture_campaign_checkpoint(directory, &mut services, &switches, parent.round)?;
    Ok((checkpoint, applied))
}

fn write_campaign_prefix_actions(
    directory: &Path,
    actions: Vec<AppliedCampaignAction>,
) -> Result<(), String> {
    let path = directory.join("topology-result.json");
    let mut result: TopologyResult = serde_json::from_slice(
        &fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    result.actions = actions;
    fs::write(
        path,
        serde_json::to_vec_pretty(&result).expect("topology result serializes"),
    )
    .map_err(|error| error.to_string())
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
            None,
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

/// Execute an autonomous campaign from one reusable, whole-topology branch
/// point. Each child restores every VM, simulated NIC/switch, UART transcript,
/// and scheduler cursor before its own operation history is injected.
fn execute_campaign(mut topology: TopologyPlan, output: &Path) -> Result<(), String> {
    let campaign = topology
        .campaign
        .take()
        .expect("campaign execution requires a campaign");
    let checkpoint =
        boot_campaign_checkpoint(&mut topology, &output.join("checkpoint"), &campaign.driver)?;
    let base = serde_json::to_vec(&topology)
        .map_err(|error| format!("cannot encode campaign base plan: {error}"))?;
    let mut checkpoints = CampaignCheckpointTree::new(checkpoint);
    let schedules = campaign_schedules(&campaign);
    if schedules.is_empty() {
        return Err("campaign produced no schedules".to_owned());
    }
    fs::create_dir_all(output.join("runs")).map_err(|error| error.to_string())?;
    let mut runs = Vec::new();
    let mut seen_markers = std::collections::BTreeSet::new();
    let mut pending = (0..schedules.len()).collect::<Vec<_>>();
    let mut observations = Vec::new();
    while runs.len() < usize::from(campaign.max_runs) && !pending.is_empty() {
        let index = runs.len();
        let (pending_index, selection) =
            select_campaign_schedule(&schedules, &pending, &observations);
        let schedule = &schedules[pending.remove(pending_index)];
        let prefix = checkpoints.checkpoint_for_schedule(&topology, &campaign, schedule, output)?;
        let mut replay: TopologyPlan = serde_json::from_slice(&base)
            .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
        apply_campaign_schedule(&mut replay, &campaign, schedule)?;
        let mut run: TopologyPlan = serde_json::from_slice(
            &serde_json::to_vec(&replay)
                .map_err(|error| format!("cannot encode campaign replay plan: {error}"))?,
        )
        .map_err(|error| format!("cannot decode campaign tail plan: {error}"))?;
        run.services
            .get_mut(&campaign.driver)
            .expect("campaign driver remains in replay plan")
            .run
            .events
            .clear();
        let run_dir = output.join("runs").join(format!("{index:03}"));
        let status = execute(
            run,
            &run_dir,
            Some(&prefix.checkpoint),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        fs::write(
            run_dir.join("replay-plan.json"),
            serde_json::to_vec_pretty(&replay).expect("campaign replay plan serializes"),
        )
        .map_err(|error| error.to_string())?;
        if run_dir.join("topology-result.json").exists() {
            write_campaign_prefix_actions(&run_dir, prefix.actions)?;
        }
        let markers = campaign_markers(&run_dir)?;
        let actions = campaign_actions(&run_dir)?;
        let novelty = markers
            .into_iter()
            .filter(|marker| seen_markers.insert(marker.clone()))
            .collect::<Vec<_>>();
        let failed = status.is_err();
        observations.push(CampaignGuidanceObservation {
            operations: schedule.operations.clone(),
            novel_markers: novelty.len(),
            failed,
        });
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
            selection,
            status: if failed { "failed" } else { "passed" },
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
            checkpoint_nodes: checkpoints.nodes(),
            checkpoint_reuses: checkpoints.reuses,
            generated_candidates: schedules.len(),
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

fn boot_campaign_checkpoint(
    topology: &mut TopologyPlan,
    directory: &Path,
    driver: &str,
) -> Result<CampaignCheckpoint, String> {
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    if let Some(runner) = &mut topology.topology_runner {
        fs::create_dir_all(directory.join("artifacts")).map_err(|error| error.to_string())?;
        let locked = lock_artifact(directory, "theseus-topology", runner)?;
        runner.path = fs::canonicalize(locked)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
    }
    let names = topology.services.keys().cloned().collect::<Vec<_>>();
    for name in &names {
        let service = &topology.services[name];
        let service_dir = directory.join("services").join(name);
        fs::create_dir_all(service_dir.join("artifacts")).map_err(|error| error.to_string())?;
        let kernel = lock_artifact(&service_dir, "kernel", &service.run.guest.kernel)?;
        let initramfs = lock_artifact(&service_dir, "initramfs", &service.run.guest.initramfs)?;
        let runtime = lock_artifact(
            &service_dir,
            "firecracker",
            &service.run.runtime.firecracker,
        )?;
        let service = topology
            .services
            .get_mut(name)
            .expect("topology service missing");
        service.run.runtime.firecracker.path = fs::canonicalize(runtime)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
        service.run.guest.kernel.path = fs::canonicalize(kernel)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
        service.run.guest.initramfs.path = fs::canonicalize(initramfs)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
    }
    fs::write(
        directory.join("replay-plan.json"),
        serde_json::to_vec_pretty(&topology).expect("checkpoint replay plan serializes"),
    )
    .map_err(|error| error.to_string())?;
    let mut switches: BTreeMap<String, SharedSimSwitch> = topology
        .networks
        .keys()
        .map(|name| (name.clone(), Arc::new(Mutex::new(SimSwitch::new()))))
        .collect();
    let mut services = BTreeMap::new();
    for name in &names {
        let service = &topology.services[name];
        let serial = directory.join("services").join(name).join("serial.log");
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
    for service in services.values() {
        service.vm.resume()?;
    }
    let boot_timeout = topology
        .services
        .values()
        .map(|service| service.run.run.timeout_secs)
        .max()
        .unwrap_or(5);
    let driver = services
        .get(driver)
        .ok_or_else(|| format!("campaign driver did not start: {driver}"))?;
    wait_for_serial_for(
        &driver.serial_logs[0],
        b"THES:M:42",
        "campaign driver serial readiness",
        Duration::from_secs(boot_timeout),
    )?;
    capture_campaign_checkpoint(directory, &mut services, &switches, 0)
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
/// reducer removes one operation at a time and restores the same complete,
/// locked topology checkpoint. It does not pretend that `sometimes` and `reachable`
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
    let checkpoint =
        boot_campaign_checkpoint(&mut topology, &output.join("checkpoint"), &campaign.driver)?;
    let base = serde_json::to_vec(&topology)
        .map_err(|error| format!("cannot encode campaign base plan: {error}"))?;
    let mut checkpoints = CampaignCheckpointTree::new(checkpoint);
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
        let prefix =
            checkpoints.checkpoint_for_schedule(&topology, &campaign, &candidate, output)?;
        let mut replay: TopologyPlan = serde_json::from_slice(&base)
            .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
        apply_campaign_schedule(&mut replay, &campaign, &candidate)?;
        let mut plan: TopologyPlan = serde_json::from_slice(
            &serde_json::to_vec(&replay)
                .map_err(|error| format!("cannot encode campaign replay plan: {error}"))?,
        )
        .map_err(|error| format!("cannot decode campaign tail plan: {error}"))?;
        plan.services
            .get_mut(&campaign.driver)
            .expect("campaign driver remains in replay plan")
            .run
            .events
            .clear();
        let result = execute(
            plan,
            &directory,
            Some(&prefix.checkpoint),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        fs::write(
            directory.join("replay-plan.json"),
            serde_json::to_vec_pretty(&replay).expect("campaign replay plan serializes"),
        )
        .map_err(|error| error.to_string())?;
        if directory.join("topology-result.json").exists() {
            write_campaign_prefix_actions(&directory, prefix.actions)?;
        }
        let _ = result;
        if property_fails_in_run(&property, &directory) {
            schedule = candidate;
        } else {
            index += 1;
        }
    }
    let prefix = checkpoints.checkpoint_for_schedule(&topology, &campaign, &schedule, output)?;
    let mut replay: TopologyPlan = serde_json::from_slice(&base)
        .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
    apply_campaign_schedule(&mut replay, &campaign, &schedule)?;
    add_counterexample_check(&mut replay, &campaign, &property)?;
    let mut final_plan: TopologyPlan = serde_json::from_slice(
        &serde_json::to_vec(&replay)
            .map_err(|error| format!("cannot encode campaign replay plan: {error}"))?,
    )
    .map_err(|error| format!("cannot decode campaign tail plan: {error}"))?;
    final_plan
        .services
        .get_mut(&campaign.driver)
        .expect("campaign driver remains in replay plan")
        .run
        .events
        .clear();
    let result = execute(
        final_plan,
        output,
        Some(&prefix.checkpoint),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    fs::write(
        output.join("replay-plan.json"),
        serde_json::to_vec_pretty(&replay).expect("campaign replay plan serializes"),
    )
    .map_err(|error| error.to_string())?;
    if output.join("topology-result.json").exists() {
        write_campaign_prefix_actions(output, prefix.actions)?;
    }
    result?;
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

#[derive(Clone, Debug)]
struct CampaignSchedule {
    operations: Vec<usize>,
    faults: Vec<usize>,
}

#[derive(Clone, Debug)]
struct CampaignGuidanceObservation {
    operations: Vec<usize>,
    novel_markers: usize,
    failed: bool,
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
    // `max_runs` bounds executed leaves, but guidance needs a broader pool to
    // choose from. Keep that pool finite even for a manifest with many
    // compatible fault combinations.
    schedules.truncate(MAX_CAMPAIGN_CANDIDATES);
    schedules
}

/// Choose the next leaf from observed serial-marker coverage rather than
/// spending the campaign budget in declaration order. An observation only
/// guides schedules that extend the exact operation history which produced it;
/// fault variants remain separate leaves and all ties fall back to the stable
/// breadth-first candidate order.
fn select_campaign_schedule(
    schedules: &[CampaignSchedule],
    pending: &[usize],
    observations: &[CampaignGuidanceObservation],
) -> (usize, String) {
    let mut selected = 0;
    let mut selected_score = 0_usize;
    let mut selected_reason = "canonical breadth-first seed".to_owned();
    for (pending_index, schedule_index) in pending.iter().enumerate() {
        let candidate = &schedules[*schedule_index];
        let mut score = 0_usize;
        let mut reason = None;
        for observation in observations {
            if !candidate.operations.starts_with(&observation.operations) {
                continue;
            }
            // A marker is the campaign's user-visible coverage signal. A
            // failed leaf is also valuable: explore its direct continuations
            // before unrelated work, without treating one failure as proof.
            let signal = observation.novel_markers.saturating_mul(1_000)
                + usize::from(observation.failed).saturating_mul(100);
            let candidate_score = signal.saturating_mul(256) + observation.operations.len();
            if candidate_score > score {
                score = candidate_score;
                reason = Some((
                    observation.novel_markers,
                    observation.failed,
                    observation.operations.len(),
                ));
            }
        }
        if score > selected_score {
            selected = pending_index;
            selected_score = score;
            selected_reason = match reason.expect("nonzero guidance has a reason") {
                (markers, _, depth) if markers > 0 => {
                    format!("extends {depth}-operation prefix with {markers} new marker(s)")
                }
                (_, true, depth) => format!("extends {depth}-operation failing prefix"),
                _ => unreachable!("nonzero guidance is marker or failure driven"),
            };
        }
    }
    (selected, selected_reason)
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
        | CampaignFaultKind::NetworkRecover
        | CampaignFaultKind::PacketFault
        | CampaignFaultKind::PacketRecover => {
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
    let events = campaign_schedule_events(campaign, schedule)?;
    let selected = schedule
        .faults
        .iter()
        .map(|index| &campaign.faults[*index])
        .collect::<Vec<_>>();
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

fn campaign_schedule_events(
    campaign: &CampaignPlan,
    schedule: &CampaignSchedule,
) -> Result<Vec<EventPlan>, String> {
    let selected = schedule
        .faults
        .iter()
        .map(|index| &campaign.faults[*index])
        .collect::<Vec<_>>();
    schedule
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
        .collect()
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
        ethertype: fault.ethertype,
        ip_protocol: fault.ip_protocol,
        source_port: fault.source_port,
        destination_port: fault.destination_port,
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
        CampaignFaultKind::PacketFault | CampaignFaultKind::PacketRecover => {
            let kind = match fault.kind {
                CampaignFaultKind::PacketFault => "packet_fault",
                CampaignFaultKind::PacketRecover => "packet_recover",
                _ => unreachable!(),
            };
            let target = match (fault.from.as_deref(), fault.to.as_deref()) {
                (Some(from), Some(to)) => format!(
                    "{}:{from}->{to}",
                    fault.network.as_deref().expect("validated action network")
                ),
                (None, None) => fault
                    .network
                    .as_deref()
                    .expect("validated action network")
                    .to_owned(),
                _ => unreachable!("validated packet target"),
            };
            format!(
                "{target}:{kind}:0x{:04x}@{}",
                fault.ethertype.expect("validated action ethertype"),
                fault.after.as_deref().expect("validated action operation")
            )
        }
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
    checkpoint: Option<&CampaignCheckpoint>,
    expected_serial: Option<BTreeMap<String, Vec<String>>>,
    expected_faults: Option<BTreeMap<String, String>>,
    expected_network: Option<String>,
    expected_actions: Option<Vec<AppliedCampaignAction>>,
    expected_storage: Option<BTreeMap<String, BTreeMap<String, String>>>,
    expected_traffic: Option<BTreeMap<String, BTreeMap<String, NetworkTraffic>>>,
    expected_virtual_time: Option<BTreeMap<String, Option<Vec<u64>>>>,
) -> Result<(), String> {
    if checkpoint.is_none() {
        if let Some(runner) = &mut topology.topology_runner {
            fs::create_dir_all(output.join("artifacts")).map_err(|error| error.to_string())?;
            let locked = lock_artifact(output, "theseus-topology", runner)?;
            runner.path = fs::canonicalize(locked)
                .map_err(|error| error.to_string())?
                .display()
                .to_string();
        }
    }
    let mut switches: BTreeMap<String, SharedSimSwitch> = topology
        .networks
        .keys()
        .map(|name| (name.clone(), Arc::new(Mutex::new(SimSwitch::new()))))
        .collect();
    let mut services = BTreeMap::new();
    let names = topology.services.keys().cloned().collect::<Vec<_>>();
    if checkpoint.is_none() {
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
    }
    fs::write(
        output.join("replay-plan.json"),
        serde_json::to_vec_pretty(&topology).unwrap(),
    )
    .map_err(|error| error.to_string())?;
    for name in &names {
        let service = &topology.services[name];
        let service_dir = output.join("services").join(name);
        fs::create_dir_all(&service_dir).map_err(|error| error.to_string())?;
        let serial = service_dir.join("serial.log");
        let (vm, serial_logs, next_fault, paused_until, faults, network_traffic, network_trace) =
            if let Some(checkpoint) = checkpoint {
                let scheduler = checkpoint
                    .scheduler
                    .get(name)
                    .ok_or_else(|| format!("checkpoint is missing scheduler state for {name}"))?;
                let serial_logs = restore_campaign_serial_logs(&service_dir, scheduler)?;
                let serial = serial_logs
                    .first()
                    .ok_or_else(|| format!("checkpoint has no serial log for {name}"))?;
                let vm = restore_service(
                    name,
                    0,
                    service,
                    Path::new(&service.run.guest.kernel.path),
                    Path::new(&service.run.guest.initramfs.path),
                    serial,
                    &switches,
                    checkpoint
                        .services
                        .get(name)
                        .ok_or_else(|| format!("checkpoint is missing VM state for {name}"))?,
                )?;
                (
                    vm,
                    serial_logs,
                    scheduler.next_fault,
                    scheduler.paused_until,
                    scheduler.faults.clone(),
                    scheduler.network_traffic.clone(),
                    scheduler.network_trace.clone(),
                )
            } else {
                (
                    build_service(
                        name,
                        0,
                        service,
                        Path::new(&service.run.guest.kernel.path),
                        Path::new(&service.run.guest.initramfs.path),
                        &serial,
                        &mut switches,
                    )?,
                    vec![serial],
                    0,
                    None,
                    Vec::new(),
                    BTreeMap::new(),
                    BTreeMap::new(),
                )
            };
        services.insert(
            name.clone(),
            ServiceRuntime {
                vm,
                serial_logs,
                next_fault,
                paused_until,
                faults,
                network_traffic,
                network_trace,
            },
        );
    }
    if let Some(checkpoint) = checkpoint {
        for (name, state) in &checkpoint.switches {
            switches
                .get(name)
                .ok_or_else(|| format!("checkpoint switch disappeared: {name}"))?
                .lock()
                .map_err(|_| "simulated switch lock poisoned".to_owned())?
                .restore_state(state.clone())
                .map_err(|error| error.to_string())?;
        }
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
    let mut round = checkpoint.map_or(0, |checkpoint| checkpoint.round);
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
        CampaignFaultKind::PacketFault | CampaignFaultKind::PacketRecover => {
            let network = action
                .network
                .as_deref()
                .ok_or_else(|| "campaign packet action has no network".to_owned())?;
            let ethertype = action
                .ethertype
                .ok_or_else(|| "campaign packet action has no ethertype".to_owned())?;
            let selector = SimNetPacketSelector {
                ethertype,
                ip_protocol: action.ip_protocol,
                source_port: action.source_port,
                destination_port: action.destination_port,
            };
            let recover = matches!(action.kind, CampaignFaultKind::PacketRecover);
            let drop_ppm = (!recover)
                .then(|| {
                    action
                        .drop_ppm
                        .ok_or_else(|| "campaign packet fault has no drop_ppm".to_owned())
                })
                .transpose()?;
            let (target, detail_prefix) = match (action.from.as_deref(), action.to.as_deref()) {
                (Some(from), Some(to)) => {
                    let destination = if to == driver_name {
                        driver.vm.network_endpoint(network)?
                    } else {
                        services
                            .get(to)
                            .ok_or_else(|| {
                                format!("campaign packet destination did not start: {to}")
                            })?
                            .vm
                            .network_endpoint(network)?
                    }
                    .ok_or_else(|| {
                        format!("campaign packet destination is not on network: {to}/{network}")
                    })?;
                    if from == driver_name {
                        driver.vm.set_network_link_packet_drop_rule(
                            network,
                            &destination,
                            selector,
                            drop_ppm,
                        )?;
                    } else {
                        services
                            .get_mut(from)
                            .ok_or_else(|| format!("campaign packet source did not start: {from}"))?
                            .vm
                            .set_network_link_packet_drop_rule(
                                network,
                                &destination,
                                selector,
                                drop_ppm,
                            )?;
                    }
                    (
                        format!("network:{network}/{from}->{to}/ethertype:0x{ethertype:04x}"),
                        "one simulated directed link".to_owned(),
                    )
                }
                (None, None) => {
                    let mut endpoints = driver
                        .vm
                        .set_network_packet_drop_rule(network, selector, drop_ppm)?;
                    for service in services.values() {
                        endpoints += service
                            .vm
                            .set_network_packet_drop_rule(network, selector, drop_ppm)?;
                    }
                    if endpoints == 0 {
                        return Err(format!(
                            "campaign packet action network has no endpoints: {network}"
                        ));
                    }
                    (
                        format!("network:{network}/ethertype:0x{ethertype:04x}"),
                        format!("{endpoints} simulated NIC endpoint(s)"),
                    )
                }
                _ => {
                    return Err(
                        "campaign packet action has an incomplete directed target".to_owned()
                    );
                }
            };
            Ok(AppliedCampaignAction {
                operation: action.operation.clone(),
                kind: if recover {
                    "packet_recover"
                } else {
                    "packet_fault"
                }
                .to_owned(),
                target,
                detail: match drop_ppm {
                    Some(drop_ppm) => {
                        format!("{detail_prefix}: drop_ppm={drop_ppm} for matching Ethernet frames")
                    }
                    None => format!("{detail_prefix}: removed matching Ethernet-frame loss rule"),
                },
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
    wait_for_serial_for(serial_log, needle, purpose, Duration::from_secs(5))
}

fn wait_for_serial_for(
    serial_log: &Path,
    needle: &[u8],
    purpose: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
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

fn service_resources(
    service: &ServicePlan,
    kernel: &Path,
    initramfs: &Path,
    serial: &Path,
) -> Result<VmResources, String> {
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
    Ok(resources)
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
    let mut resources = service_resources(service, kernel, initramfs, serial)?;
    let mut simulated_storage = Vec::new();
    let mut simulated_networks = Vec::new();
    let mut network_endpoints = BTreeMap::new();
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
        simulated_storage.push(storage.id.clone());
    }
    for network in &service.networks {
        let switch = switches
            .get(network)
            .ok_or_else(|| format!("service {name}: unknown network {network}"))?
            .clone();
        let endpoint = format!("{network}/{name}-{instance}");
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
                endpoint.clone(),
                None,
                RateLimiter::default(),
                RateLimiter::default(),
                None,
            )
            .map_err(|error| error.to_string())?,
        ));
        resources.net_builder.add_device(net.clone());
        simulated_networks.push((network.clone(), format!("net-{network}")));
        network_endpoints.insert(network.clone(), endpoint);
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
        network_endpoints,
    })
}

fn restore_service(
    name: &str,
    instance: usize,
    service: &ServicePlan,
    kernel: &Path,
    initramfs: &Path,
    serial: &Path,
    switches: &BTreeMap<String, SharedSimSwitch>,
    checkpoint: &ServiceVmCheckpoint,
) -> Result<ServiceVm, String> {
    let mut resources = service_resources(service, kernel, initramfs, serial)?;
    let networks = service
        .networks
        .iter()
        .map(|network| (network.clone(), format!("net-{network}")))
        .collect::<Vec<_>>();
    let network_endpoints = service
        .networks
        .iter()
        .map(|network| (network.clone(), format!("{network}/{name}-{instance}")))
        .collect::<BTreeMap<_, _>>();
    let storage = service
        .run
        .storage
        .iter()
        .map(|storage| storage.id.clone())
        .collect();
    let mut event_manager = EventManager::new().map_err(|error| error.to_string())?;
    let vmm = restore_from_snapshot(
        &InstanceInfo::default(),
        &mut event_manager,
        &get_empty_filters(),
        &LoadSnapshotParams {
            snapshot_path: checkpoint.snapshot_path.clone(),
            mem_backend: MemBackendConfig {
                backend_path: checkpoint.memory_path.clone(),
                backend_type: MemBackendType::File,
            },
            track_dirty_pages: false,
            resume_vm: false,
            network_overrides: Vec::new(),
            vsock_override: None,
            clock_realtime: false,
            huge_pages: SnapshotLoadHugePageConfig::Snapshot,
        },
        &mut resources,
    )
    .map_err(|error| error.to_string())?;
    let vm = ServiceVm {
        vmm,
        event_manager,
        storage,
        networks,
        network_endpoints,
    };
    {
        let mut vmm = vm.vmm.lock().expect("VMM lock poisoned");
        for (network, id) in &vm.networks {
            let switch = switches
                .get(network)
                .ok_or_else(|| format!("checkpoint network disappeared: {network}"))?
                .clone();
            let endpoint = vm
                .network_endpoints
                .get(network)
                .ok_or_else(|| format!("checkpoint endpoint disappeared: {network}"))?
                .clone();
            if !vmm.attach_simulated_network(id, switch, endpoint) {
                return Err(format!("cannot reconnect restored network: {network}"));
            }
        }
    }
    vm.restore_network_states(checkpoint.networks.clone())?;
    Ok(vm)
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

    #[test]
    fn campaign_prefix_key_includes_barrier_actions() {
        let ordinary = vec![EventPlan {
            data_hex: "70696e670a".to_owned(),
            checkpoint: Some("THES:CHECKPOINT:ping".to_owned()),
            actions: Vec::new(),
        }];
        let faulted = vec![EventPlan {
            data_hex: "70696e670a".to_owned(),
            checkpoint: Some("THES:CHECKPOINT:ping".to_owned()),
            actions: vec![CampaignAction {
                operation: "ping".to_owned(),
                kind: CampaignFaultKind::Partition,
                service: None,
                network: Some("backplane".to_owned()),
                from: None,
                to: None,
                drive: None,
                error_ppm: None,
                latency_rounds: None,
                torn_write_bytes: None,
                corrupt_read_xor: None,
                ethertype: None,
                ip_protocol: None,
                source_port: None,
                destination_port: None,
                drop_ppm: None,
                duplicate_ppm: None,
                corrupt_ppm: None,
                jitter_rounds: None,
                tx_bytes_per_round: None,
                mtu_bytes: None,
                tx_queue_frames: None,
                rx_queue_frames: None,
            }],
        }];

        assert_eq!(
            campaign_prefix_key(&ordinary),
            campaign_prefix_key(&ordinary)
        );
        assert_ne!(
            campaign_prefix_key(&ordinary),
            campaign_prefix_key(&faulted)
        );
    }

    #[test]
    fn campaign_selection_extends_a_marker_novel_prefix_before_canonical_work() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![0],
                faults: Vec::new(),
            },
            CampaignSchedule {
                operations: vec![1],
                faults: Vec::new(),
            },
            CampaignSchedule {
                operations: vec![0, 1],
                faults: Vec::new(),
            },
        ];
        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[1, 2],
            &[CampaignGuidanceObservation {
                operations: vec![0],
                novel_markers: 2,
                failed: false,
            }],
        );

        assert_eq!(selected, 1);
        assert_eq!(reason, "extends 1-operation prefix with 2 new marker(s)");
    }

    #[test]
    fn campaign_selection_keeps_canonical_order_without_a_signal() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![0],
                faults: Vec::new(),
            },
            CampaignSchedule {
                operations: vec![1],
                faults: Vec::new(),
            },
        ];
        let (selected, reason) = select_campaign_schedule(&schedules, &[0, 1], &[]);

        assert_eq!(selected, 0);
        assert_eq!(reason, "canonical breadth-first seed");
    }

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
            ethertype: None,
            ip_protocol: None,
            source_port: None,
            destination_port: None,
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
    fn campaign_packet_action_keeps_its_ethertype_target() {
        let mut fault = campaign_fault(CampaignFaultKind::PacketFault);
        fault.from = Some("api".to_owned());
        fault.to = Some("replica".to_owned());
        fault.ethertype = Some(0x0800);
        fault.drop_ppm = Some(1_000_000);

        let action = campaign_action(&fault).unwrap();
        assert!(matches!(action.kind, CampaignFaultKind::PacketFault));
        assert_eq!(action.ethertype, Some(0x0800));
        assert_eq!(action.drop_ppm, Some(1_000_000));
        assert_eq!(
            campaign_fault_name(&fault),
            "backplane:api->replica:packet_fault:0x0800@write"
        );
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
