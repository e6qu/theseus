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

use regex::bytes::Regex;
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
    stages: Vec<String>,
    #[serde(default)]
    faults: Vec<CampaignFault>,
    #[serde(default)]
    properties: Vec<CampaignProperty>,
    max_runs: u16,
    #[serde(default = "default_campaign_faults_per_run")]
    max_faults_per_run: u8,
    #[serde(default = "default_campaign_operations_per_run")]
    max_operations_per_run: u8,
}

fn default_campaign_faults_per_run() -> u8 {
    2
}

fn default_campaign_operations_per_run() -> u8 {
    3
}

#[derive(Debug, Deserialize, Serialize)]
struct CampaignOperation {
    name: String,
    input_hex: String,
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
    #[serde(default)]
    requires_markers: Vec<String>,
    #[serde(default)]
    excludes_markers: Vec<String>,
    #[serde(default)]
    requires_serial: Option<OperationSerialGuard>,
    #[serde(default)]
    excludes_serial: Option<OperationSerialGuard>,
    #[serde(default)]
    requires_serial_all: Vec<OperationSerialGuard>,
    #[serde(default)]
    excludes_serial_any: Vec<OperationSerialGuard>,
    #[serde(default)]
    requires_serial_joins: Vec<SerialJoin>,
    #[serde(default)]
    excludes_serial_joins: Vec<SerialJoin>,
    #[serde(default)]
    requires_serial_evidence: Option<SerialEvidence>,
    #[serde(default)]
    excludes_serial_evidence: Option<SerialEvidence>,
    #[serde(default)]
    max_uses: Option<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct OperationSerialGuard {
    #[serde(default)]
    service: Option<String>,
    #[serde(flatten)]
    predicate: SerialPredicate,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CampaignProperty {
    name: String,
    kind: PropertyKind,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    contains_all: Vec<String>,
    #[serde(default)]
    contains_any: Vec<String>,
    #[serde(default)]
    contains_none: Vec<String>,
    #[serde(default)]
    predicate: Option<SerialPredicate>,
    #[serde(default)]
    requires_serial_all: Vec<OperationSerialGuard>,
    #[serde(default)]
    requires_serial_any: Vec<OperationSerialGuard>,
    #[serde(default)]
    excludes_serial_any: Vec<OperationSerialGuard>,
    #[serde(default)]
    requires_serial_correlations: Vec<SerialCorrelation>,
    #[serde(default)]
    requires_serial_joins: Vec<SerialJoin>,
    #[serde(default)]
    requires_serial_evidence: Option<SerialEvidence>,
    #[serde(default)]
    excludes_serial_evidence: Option<SerialEvidence>,
    #[serde(default)]
    service: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SerialCorrelation {
    capture: JsonCorrelationEndpoint,
    equals: JsonCorrelationEndpoint,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JsonCorrelationEndpoint {
    #[serde(default)]
    service: Option<String>,
    /// Read older replay plans that used one `pointer`; new plans use
    /// `pointers` so a join can compare a composite key.
    #[serde(default)]
    pointer: Option<String>,
    #[serde(default)]
    pointers: Vec<String>,
    json: JsonPredicate,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SerialJoin {
    endpoints: Vec<JsonCorrelationEndpoint>,
    #[serde(default)]
    quantifier: SerialJoinQuantifier,
    #[serde(default)]
    occurs: Option<SerialMatchCount>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum SerialJoinQuantifier {
    Any,
    Every,
}

impl Default for SerialJoinQuantifier {
    fn default() -> Self {
        Self::Any
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SerialRelation {
    left: JsonCorrelationEndpoint,
    right: JsonCorrelationEndpoint,
    operator: JsonRelationOperator,
    #[serde(default)]
    order: Option<SerialRelationOrder>,
    #[serde(default)]
    quantifier: SerialJoinQuantifier,
    #[serde(default)]
    occurs: Option<SerialMatchCount>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SerialPath {
    #[serde(default)]
    service: Option<String>,
    pointers: Vec<String>,
    steps: Vec<JsonPredicate>,
    #[serde(default)]
    quantifier: SerialJoinQuantifier,
    #[serde(default)]
    occurs: Option<SerialMatchCount>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum SerialRelationOrder {
    Before,
    After,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SerialMatchCount {
    #[serde(default)]
    exactly: Option<u64>,
    #[serde(default)]
    at_least: Option<u64>,
    #[serde(default)]
    at_most: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonRelationOperator {
    Equals,
    NotEquals,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SerialEvidence {
    #[serde(default)]
    all: Vec<SerialEvidence>,
    #[serde(default)]
    any: Vec<SerialEvidence>,
    #[serde(default)]
    none: Vec<SerialEvidence>,
    #[serde(default)]
    guard: Option<OperationSerialGuard>,
    #[serde(default)]
    correlation: Option<SerialCorrelation>,
    #[serde(default)]
    join: Option<SerialJoin>,
    #[serde(default)]
    relation: Option<SerialRelation>,
    #[serde(default)]
    path: Option<SerialPath>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SerialPredicate {
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    matches: Option<String>,
    #[serde(default)]
    json: Option<JsonPredicate>,
    #[serde(default)]
    all: Vec<SerialPredicate>,
    #[serde(default)]
    any: Vec<SerialPredicate>,
    #[serde(default)]
    none: Vec<SerialPredicate>,
    #[serde(default)]
    sequence: Vec<SerialPredicate>,
    #[serde(default)]
    occurs: Option<SerialOccurrence>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SerialOccurrence {
    predicate: Box<SerialPredicate>,
    #[serde(default)]
    exactly: Option<u64>,
    #[serde(default)]
    at_least: Option<u64>,
    #[serde(default)]
    at_most: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct JsonPredicate {
    #[serde(default)]
    fields: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "where")]
    where_: Vec<JsonCondition>,
    #[serde(default)]
    arrays: Vec<JsonArrayPredicate>,
    #[serde(default)]
    capture: BTreeMap<String, String>,
    #[serde(default)]
    equals_capture: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct JsonArrayPredicate {
    pointer: String,
    #[serde(default)]
    any: Option<Box<JsonPredicate>>,
    #[serde(default)]
    all: Option<Box<JsonPredicate>>,
    #[serde(default)]
    none: Option<Box<JsonPredicate>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct JsonCondition {
    pointer: String,
    #[serde(default)]
    equals: Option<serde_json::Value>,
    #[serde(default)]
    matches: Option<String>,
    #[serde(default)]
    greater_than: Option<f64>,
    #[serde(default)]
    greater_than_or_equal: Option<f64>,
    #[serde(default)]
    less_than: Option<f64>,
    #[serde(default)]
    less_than_or_equal: Option<f64>,
    #[serde(default)]
    exists: Option<bool>,
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
    marker_guard_rejections: usize,
    serial_guard_rejections: usize,
    unique_topology_states: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay_verification: Option<CampaignReplayVerification>,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    program_counters: BTreeMap<String, Vec<String>>,
    state_sha256: String,
    state_novel: bool,
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

#[derive(Debug, Serialize)]
struct CampaignReplayVerification {
    status: &'static str,
    detail: String,
}

#[derive(Debug, Deserialize)]
struct RecordedCampaignResult {
    #[serde(default)]
    generated_candidates: usize,
    runs: Vec<RecordedCampaignRun>,
}

#[derive(Debug, Deserialize)]
struct RecordedCampaignRun {
    operations: Vec<String>,
    #[serde(default)]
    fault: Option<String>,
    #[serde(default)]
    faults: Vec<String>,
    #[serde(default)]
    actions: Vec<AppliedCampaignAction>,
    #[serde(default)]
    selection: String,
    #[serde(default)]
    program_counters: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    novelty: Vec<String>,
    #[serde(default)]
    state_sha256: String,
    #[serde(default)]
    state_novel: bool,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Serialize)]
struct CampaignMinimization {
    property: String,
    original_operations: Vec<String>,
    minimized_operations: Vec<String>,
    original_faults: Vec<String>,
    minimized_faults: Vec<String>,
    operation_attempts: usize,
    fault_attempts: usize,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    contains_all: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    contains_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    contains_none: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    predicate: Option<SerialPredicate>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckKind {
    SerialContains,
    SerialNotContains,
    SerialPropertyMatches,
    SerialPropertyDoesNotMatch,
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
    program_counters: Vec<u64>,
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

enum CampaignPrefixResult {
    Ready(CampaignPrefixCheckpoint),
    MarkerGuardRejected,
    SerialGuardRejected,
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

    fn paused_program_counters(&self) -> Result<Vec<u64>, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .paused_vcpu_program_counters()
            .map_err(|error| error.to_string())
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
                    program_counters: service.vm.paused_program_counters()?,
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

    /// Materialize a history one operation at a time. Each state and marker
    /// guard sees the exact restored parent checkpoint, so an invalid earlier
    /// operation can never become a prefix for a later leaf.
    fn checkpoint_for_guarded_schedule(
        &mut self,
        topology: &TopologyPlan,
        campaign: &CampaignPlan,
        schedule: &CampaignSchedule,
        directory: &Path,
    ) -> Result<CampaignPrefixResult, String> {
        let events = campaign_schedule_events(campaign, schedule)?;
        let mut prefix = Vec::new();
        let mut parent = CampaignPrefixCheckpoint {
            checkpoint: self.root.clone(),
            actions: Vec::new(),
        };
        for (index, (operation, event)) in schedule.operations.iter().zip(events).enumerate() {
            if !campaign_operation_is_ready(campaign, &schedule.operations[..index], *operation)
                || !campaign_operation_marker_guards_are_ready(
                    campaign,
                    &parent.checkpoint,
                    *operation,
                )
            {
                return Ok(CampaignPrefixResult::MarkerGuardRejected);
            }
            if !campaign_operation_serial_guards_are_ready(campaign, &parent.checkpoint, *operation)
            {
                return Ok(CampaignPrefixResult::SerialGuardRejected);
            }
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
        Ok(CampaignPrefixResult::Ready(parent))
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
    let recorded_campaign = recorded_campaign_result(Path::new(plan))?;
    let (
        expected_serial,
        expected_faults,
        expected_network,
        expected_actions,
        expected_storage,
        expected_traffic,
        expected_virtual_time,
    ) = if recorded_campaign.is_some() {
        (None, None, None, None, None, None, None)
    } else {
        (
            recorded_serial_fingerprints(Path::new(plan), &service_names)?,
            recorded_fault_fingerprints(Path::new(plan), &service_names)?,
            recorded_network_fingerprint(Path::new(plan))?,
            recorded_campaign_actions(Path::new(plan))?,
            recorded_storage_fingerprints(Path::new(plan), &service_names)?,
            recorded_network_traffic(Path::new(plan), &service_names)?,
            recorded_virtual_times(Path::new(plan), &service_names)?,
        )
    };
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
            execute_campaign(topology, &output, recorded_campaign.as_ref())
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
fn execute_campaign(
    mut topology: TopologyPlan,
    output: &Path,
    recorded: Option<&RecordedCampaignResult>,
) -> Result<(), String> {
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
    let replay_schedules = recorded
        .map(|recorded| recorded_campaign_schedules(&campaign, recorded))
        .transpose()?;
    fs::create_dir_all(output.join("runs")).map_err(|error| error.to_string())?;
    let mut runs = Vec::new();
    let mut seen_markers = std::collections::BTreeSet::new();
    let mut seen_topology_states = std::collections::BTreeSet::new();
    let mut pending = (0..schedules.len()).collect::<Vec<_>>();
    let mut observations = Vec::new();
    let mut replay_mismatches = Vec::new();
    let mut marker_guard_rejections = 0_usize;
    let mut serial_guard_rejections = 0_usize;
    while runs.len()
        < replay_schedules
            .as_ref()
            .map_or(usize::from(campaign.max_runs), Vec::len)
        && (replay_schedules.is_some() || !pending.is_empty())
    {
        let index = runs.len();
        let (schedule, selection, expected) = if let Some(replay_schedules) = &replay_schedules {
            let expected = &recorded
                .expect("replay schedules have recorded evidence")
                .runs[index];
            (
                replay_schedules[index].clone(),
                if expected.selection.is_empty() {
                    "recorded campaign schedule".to_owned()
                } else {
                    expected.selection.clone()
                },
                Some(expected),
            )
        } else {
            let (pending_index, selection) =
                select_campaign_schedule(&schedules, &pending, &observations);
            (
                schedules[pending.remove(pending_index)].clone(),
                selection,
                None,
            )
        };
        // Guards inspect each exact restored parent checkpoint. A fault after
        // an earlier operation is visible to the next operation's guard, just
        // as it is to the guest; impossible prefixes never become leaves.
        let prefix = match checkpoints
            .checkpoint_for_guarded_schedule(&topology, &campaign, &schedule, output)?
        {
            CampaignPrefixResult::Ready(prefix) => prefix,
            rejection => {
                if expected.is_some() {
                    return Err(format!(
                        "recorded campaign history no longer satisfies its operation guards: {}",
                        schedule
                            .operations
                            .iter()
                            .map(|operation| campaign.operations[*operation].name.as_str())
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    ));
                }
                match rejection {
                    CampaignPrefixResult::MarkerGuardRejected => marker_guard_rejections += 1,
                    CampaignPrefixResult::SerialGuardRejected => serial_guard_rejections += 1,
                    CampaignPrefixResult::Ready(_) => unreachable!("a campaign prefix is ready"),
                }
                continue;
            }
        };
        let mut replay: TopologyPlan = serde_json::from_slice(&base)
            .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
        apply_campaign_schedule(&mut replay, &campaign, &schedule)?;
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
        let program_counters = campaign_checkpoint_program_counters(&prefix.checkpoint);
        let state_sha256 = campaign_topology_state_sha256(&run_dir, &program_counters)?;
        let state_novel = seen_topology_states.insert(state_sha256.clone());
        let novelty = markers
            .into_iter()
            .filter(|marker| seen_markers.insert(marker.clone()))
            .collect::<Vec<_>>();
        let failed = status.is_err();
        observations.push(CampaignGuidanceObservation {
            operations: schedule.operations.clone(),
            novel_markers: novelty.len(),
            novel_state: state_novel,
            failed,
        });
        let run = CampaignRun {
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
            program_counters,
            state_sha256,
            state_novel,
            status: if failed { "failed" } else { "passed" },
            novelty,
        };
        if let Some(expected) = expected {
            let mismatches = campaign_replay_mismatches(expected, &run);
            if !mismatches.is_empty() {
                replay_mismatches.push(format!("run {index}: {}", mismatches.join(", ")));
            }
        }
        runs.push(run);
    }
    if runs.is_empty() {
        return Err("campaign produced no schedules after marker guards".to_owned());
    }
    let properties = evaluate_campaign_properties(&campaign, output, &runs)?;
    let passed = runs.iter().all(|run| run.status == "passed")
        && properties
            .iter()
            .all(|property| property.status == "passed");
    let replay_verified = recorded.is_none()
        || (replay_mismatches.is_empty()
            && recorded.is_some_and(|recorded| {
                recorded.generated_candidates == 0
                    || recorded.generated_candidates == schedules.len()
            }));
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
            status: if passed && replay_verified {
                "passed"
            } else {
                "failed"
            },
            driver: replay
                .campaign
                .as_ref()
                .expect("campaign remains in replay plan")
                .driver
                .clone(),
            checkpoint_nodes: checkpoints.nodes(),
            checkpoint_reuses: checkpoints.reuses,
            generated_candidates: schedules.len(),
            marker_guard_rejections,
            serial_guard_rejections,
            unique_topology_states: seen_topology_states.len(),
            replay_verification: recorded.map(|recorded| CampaignReplayVerification {
                status: if replay_verified { "passed" } else { "failed" },
                detail: if replay_verified {
                    format!("{} recorded campaign timelines reproduced", runs.len())
                } else {
                    let mut detail = replay_mismatches.clone();
                    if recorded.generated_candidates != 0
                        && recorded.generated_candidates != schedules.len()
                    {
                        detail.push("generated candidate corpus changed".to_owned());
                    }
                    detail.join("; ")
                },
            }),
            runs,
            properties,
        })
        .expect("campaign result serializes"),
    )
    .map_err(|error| error.to_string())?;
    if !replay_verified {
        Err(format!(
            "campaign replay verification failed; inspect {}",
            output.display()
        ))
    } else if passed {
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

/// Delta-debug the first timeline that violates an individual property. The
/// reducer uses coarse-to-fine delta debugging for operations, then selected
/// faults, restoring the same complete, locked topology checkpoint for every
/// attempt. It does not pretend that `sometimes` and `reachable` failures have
/// a single counterexample: those properties fail because the corpus has no
/// witness, so their full first schedule is retained.
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
    let original_faults = campaign_fault_names(&campaign, &schedule.faults);
    let attempts = output.join("minimization-attempts");
    fs::create_dir_all(&attempts).map_err(|error| error.to_string())?;
    let mut attempt = 0_usize;
    let faults = schedule.faults.clone();
    let (operations, operation_attempts) =
        minimize_campaign_items(schedule.operations.clone(), 1, |operations| {
            let candidate = CampaignSchedule {
                operations: operations.to_vec(),
                faults: faults.clone(),
            };
            let directory = attempts.join(format!("{attempt:03}"));
            attempt += 1;
            execute_campaign_minimization_attempt(
                &topology,
                &campaign,
                &base,
                &mut checkpoints,
                &candidate,
                &property,
                output,
                &directory,
            )
        })?;
    schedule.operations = operations;
    let operations = schedule.operations.clone();
    let (faults, fault_attempts) = minimize_campaign_items(schedule.faults.clone(), 0, |faults| {
        let candidate = CampaignSchedule {
            operations: operations.clone(),
            faults: faults.to_vec(),
        };
        let directory = attempts.join(format!("{attempt:03}"));
        attempt += 1;
        execute_campaign_minimization_attempt(
            &topology,
            &campaign,
            &base,
            &mut checkpoints,
            &candidate,
            &property,
            output,
            &directory,
        )
    })?;
    schedule.faults = faults;
    let prefix = match checkpoints
        .checkpoint_for_guarded_schedule(&topology, &campaign, &schedule, output)?
    {
        CampaignPrefixResult::Ready(prefix) => prefix,
        CampaignPrefixResult::MarkerGuardRejected | CampaignPrefixResult::SerialGuardRejected => {
            return Err(
                "minimized campaign history no longer satisfies its operation guards".to_owned(),
            );
        }
    };
    let mut replay: TopologyPlan = serde_json::from_slice(&base)
        .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
    apply_campaign_schedule(&mut replay, &campaign, &schedule)?;
    add_counterexample_check(&mut replay, &property)?;
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
            original_faults,
            minimized_faults: campaign_fault_names(&campaign, &schedule.faults),
            operation_attempts,
            fault_attempts,
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

/// Apply classic `ddmin` deletion passes. Start with broad contiguous chunks,
/// then refine only when no chunk can be removed. At its finest granularity the
/// result is still 1-minimal, while long irrelevant prefixes disappear in one
/// replay attempt.
fn minimize_campaign_items<F>(
    mut items: Vec<usize>,
    minimum_len: usize,
    mut property_fails: F,
) -> Result<(Vec<usize>, usize), String>
where
    F: FnMut(&[usize]) -> Result<bool, String>,
{
    let mut granularity = 2_usize;
    let mut attempts = 0_usize;
    while items.len() > minimum_len {
        let chunk_len = items.len().div_ceil(granularity);
        let mut reduced = None;
        for start in (0..items.len()).step_by(chunk_len) {
            let end = (start + chunk_len).min(items.len());
            if items.len() - (end - start) < minimum_len {
                continue;
            }
            let mut candidate = items.clone();
            candidate.drain(start..end);
            attempts += 1;
            if property_fails(&candidate)? {
                reduced = Some(candidate);
                break;
            }
        }
        if let Some(candidate) = reduced {
            items = candidate;
            granularity = granularity.saturating_sub(1).max(2);
        } else if granularity >= items.len() {
            break;
        } else {
            granularity = (granularity * 2).min(items.len());
        }
    }
    Ok((items, attempts))
}

/// Run one candidate from its longest reusable operation/action checkpoint.
/// Keep the candidate's normal replay plan and action evidence even when the
/// property does not survive the attempted reduction.
fn execute_campaign_minimization_attempt(
    topology: &TopologyPlan,
    campaign: &CampaignPlan,
    base: &[u8],
    checkpoints: &mut CampaignCheckpointTree,
    schedule: &CampaignSchedule,
    property: &CampaignProperty,
    output: &Path,
    directory: &Path,
) -> Result<bool, String> {
    let prefix = match checkpoints
        .checkpoint_for_guarded_schedule(topology, campaign, schedule, output)?
    {
        CampaignPrefixResult::Ready(prefix) => prefix,
        CampaignPrefixResult::MarkerGuardRejected | CampaignPrefixResult::SerialGuardRejected => {
            return Ok(false);
        }
    };
    let mut replay: TopologyPlan = serde_json::from_slice(base)
        .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
    apply_campaign_schedule(&mut replay, campaign, schedule)?;
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
        directory,
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
        write_campaign_prefix_actions(directory, prefix.actions)?;
    }
    let _ = result;
    Ok(property_fails_in_run(property, directory))
}

fn add_counterexample_check(
    topology: &mut TopologyPlan,
    property: &CampaignProperty,
) -> Result<(), String> {
    // An unscoped property may have failed in any service. The locked bundle is
    // still re-evaluated below, but no one service check can prove it.
    if !property.requires_serial_all.is_empty()
        || !property.requires_serial_any.is_empty()
        || !property.excludes_serial_any.is_empty()
        || !property.requires_serial_correlations.is_empty()
        || !property.requires_serial_joins.is_empty()
        || property.requires_serial_evidence.is_some()
        || property.excludes_serial_evidence.is_some()
    {
        return Ok(());
    }
    let Some(service) = property.service.as_ref() else {
        return Ok(());
    };
    let check = topology
        .services
        .get_mut(service)
        .ok_or_else(|| format!("property service disappeared: {service}"))?;
    let compound = !property.contains_all.is_empty()
        || !property.contains_any.is_empty()
        || !property.contains_none.is_empty()
        || property.predicate.is_some();
    let kind = match (property.kind, compound) {
        (PropertyKind::Unreachable, false) => CheckKind::SerialContains,
        (PropertyKind::Always | PropertyKind::Sometimes | PropertyKind::Reachable, false) => {
            CheckKind::SerialNotContains
        }
        (PropertyKind::Unreachable, true) => CheckKind::SerialPropertyMatches,
        (PropertyKind::Always | PropertyKind::Sometimes | PropertyKind::Reachable, true) => {
            CheckKind::SerialPropertyDoesNotMatch
        }
    };
    check.run.checks.push(CheckPlan {
        name: format!("counterexample: {}", property.name),
        kind,
        value: property.contains.clone().unwrap_or_default(),
        contains_all: property.contains_all.clone(),
        contains_any: property.contains_any.clone(),
        contains_none: property.contains_none.clone(),
        predicate: property.predicate.clone(),
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
                contains_all: property.contains_all.clone(),
                contains_any: property.contains_any.clone(),
                contains_none: property.contains_none.clone(),
                predicate: property.predicate.clone(),
                requires_serial_all: property.requires_serial_all.clone(),
                requires_serial_any: property.requires_serial_any.clone(),
                excludes_serial_any: property.excludes_serial_any.clone(),
                requires_serial_correlations: property.requires_serial_correlations.clone(),
                requires_serial_joins: property.requires_serial_joins.clone(),
                requires_serial_evidence: property.requires_serial_evidence.clone(),
                excludes_serial_evidence: property.excludes_serial_evidence.clone(),
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
    let primary_matches = campaign_property_services(run, property.service.as_deref())
        .into_iter()
        .any(|service| campaign_serial_matches_property(run, &service, property));
    primary_matches
        && property
            .requires_serial_all
            .iter()
            .all(|guard| campaign_serial_guard_matches_property(run, property, guard))
        && (property.requires_serial_any.is_empty()
            || property
                .requires_serial_any
                .iter()
                .any(|guard| campaign_serial_guard_matches_property(run, property, guard)))
        && property
            .excludes_serial_any
            .iter()
            .all(|guard| !campaign_serial_guard_matches_property(run, property, guard))
        && property
            .requires_serial_correlations
            .iter()
            .all(|correlation| serial_correlation_matches_property(run, property, correlation))
        && property
            .requires_serial_joins
            .iter()
            .all(|join| serial_join_matches_property(run, property, join))
        && property
            .requires_serial_evidence
            .as_ref()
            .map(|evidence| serial_evidence_matches_property(run, property, evidence))
            .unwrap_or(true)
        && property
            .excludes_serial_evidence
            .as_ref()
            .map(|evidence| !serial_evidence_matches_property(run, property, evidence))
            .unwrap_or(true)
}

fn campaign_property_services(run: &Path, service: Option<&str>) -> Vec<String> {
    service
        .map(|service| vec![service.to_owned()])
        .unwrap_or_else(|| {
            fs::read_dir(run.join("services"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
}

#[derive(Clone, Debug)]
struct CampaignSchedule {
    operations: Vec<usize>,
    faults: Vec<usize>,
}

fn campaign_operation_marker_guards_are_ready(
    campaign: &CampaignPlan,
    checkpoint: &CampaignCheckpoint,
    operation: usize,
) -> bool {
    let markers = campaign_checkpoint_markers(checkpoint);
    let candidate = &campaign.operations[operation];
    candidate
        .requires_markers
        .iter()
        .all(|marker| markers.contains(marker))
        && candidate
            .excludes_markers
            .iter()
            .all(|marker| !markers.contains(marker))
}

fn campaign_operation_serial_guards_are_ready(
    campaign: &CampaignPlan,
    checkpoint: &CampaignCheckpoint,
    operation: usize,
) -> bool {
    let candidate = &campaign.operations[operation];
    candidate
        .requires_serial
        .as_ref()
        .map(|guard| campaign_serial_guard_matches(checkpoint, &campaign.driver, guard))
        .unwrap_or(true)
        && candidate
            .requires_serial_all
            .iter()
            .all(|guard| campaign_serial_guard_matches(checkpoint, &campaign.driver, guard))
        && candidate
            .excludes_serial
            .as_ref()
            .map(|guard| !campaign_serial_guard_matches(checkpoint, &campaign.driver, guard))
            .unwrap_or(true)
        && candidate
            .excludes_serial_any
            .iter()
            .all(|guard| !campaign_serial_guard_matches(checkpoint, &campaign.driver, guard))
        && candidate
            .requires_serial_joins
            .iter()
            .all(|join| campaign_serial_join_matches(checkpoint, &campaign.driver, join))
        && candidate
            .excludes_serial_joins
            .iter()
            .all(|join| !campaign_serial_join_matches(checkpoint, &campaign.driver, join))
        && candidate
            .requires_serial_evidence
            .as_ref()
            .map(|evidence| {
                campaign_serial_evidence_matches(checkpoint, &campaign.driver, evidence)
            })
            .unwrap_or(true)
        && candidate
            .excludes_serial_evidence
            .as_ref()
            .map(|evidence| {
                !campaign_serial_evidence_matches(checkpoint, &campaign.driver, evidence)
            })
            .unwrap_or(true)
}

fn campaign_serial_guard_matches(
    checkpoint: &CampaignCheckpoint,
    driver: &str,
    guard: &OperationSerialGuard,
) -> bool {
    let serial = campaign_checkpoint_serial(checkpoint, guard.service.as_deref().unwrap_or(driver));
    serial_matches_nested_predicate(&serial, &guard.predicate)
}

fn campaign_serial_join_matches(
    checkpoint: &CampaignCheckpoint,
    driver: &str,
    join: &SerialJoin,
) -> bool {
    let Some((first, rest)) = join.endpoints.split_first() else {
        return false;
    };
    let candidates = campaign_join_endpoint_values(checkpoint, driver, first);
    let peer_values = rest
        .iter()
        .map(|endpoint| campaign_join_endpoint_values(checkpoint, driver, endpoint))
        .collect::<Vec<_>>();
    let matches = candidates
        .iter()
        .filter(|candidate| {
            peer_values
                .iter()
                .all(|values| values.iter().any(|value| value == *candidate))
        })
        .cloned()
        .collect();
    serial_match_requirements_met(candidates, matches, join.quantifier, join.occurs.as_ref())
}

fn distinct_json_values(values: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut distinct = Vec::new();
    for value in values {
        if !distinct.iter().any(|existing| existing == &value) {
            distinct.push(value);
        }
    }
    distinct
}

fn serial_match_requirements_met(
    candidates: Vec<serde_json::Value>,
    matches: Vec<serde_json::Value>,
    quantifier: SerialJoinQuantifier,
    occurs: Option<&SerialMatchCount>,
) -> bool {
    let candidates = distinct_json_values(candidates);
    let matches = distinct_json_values(matches);
    if candidates.is_empty() {
        return false;
    }
    let quantified = match quantifier {
        SerialJoinQuantifier::Any => !matches.is_empty(),
        SerialJoinQuantifier::Every => matches.len() == candidates.len(),
    };
    let count_matches = occurs.is_none_or(|count| {
        let matches = matches.len() as u64;
        count.exactly.is_none_or(|exactly| matches == exactly)
            && count.at_least.is_none_or(|at_least| matches >= at_least)
            && count.at_most.is_none_or(|at_most| matches <= at_most)
    });
    quantified && count_matches
}

fn campaign_serial_correlation_matches(
    checkpoint: &CampaignCheckpoint,
    driver: &str,
    correlation: &SerialCorrelation,
) -> bool {
    let captures = campaign_join_endpoint_values(checkpoint, driver, &correlation.capture);
    !captures.is_empty()
        && campaign_join_endpoint_values(checkpoint, driver, &correlation.equals)
            .into_iter()
            .any(|value| captures.iter().any(|capture| capture == &value))
}

fn json_relation_matches(
    left: &serde_json::Value,
    right: &serde_json::Value,
    operator: JsonRelationOperator,
) -> bool {
    match operator {
        JsonRelationOperator::Equals => left == right,
        JsonRelationOperator::NotEquals => left != right,
        JsonRelationOperator::GreaterThan
        | JsonRelationOperator::GreaterThanOrEqual
        | JsonRelationOperator::LessThan
        | JsonRelationOperator::LessThanOrEqual => {
            let number = |value: &serde_json::Value| {
                value.as_array().and_then(|values| match values.as_slice() {
                    [value] => value.as_f64(),
                    _ => None,
                })
            };
            let Some((left, right)) = number(left).zip(number(right)) else {
                return false;
            };
            match operator {
                JsonRelationOperator::GreaterThan => left > right,
                JsonRelationOperator::GreaterThanOrEqual => left >= right,
                JsonRelationOperator::LessThan => left < right,
                JsonRelationOperator::LessThanOrEqual => left <= right,
                JsonRelationOperator::Equals | JsonRelationOperator::NotEquals => unreachable!(),
            }
        }
    }
}

fn campaign_serial_relation_matches(
    checkpoint: &CampaignCheckpoint,
    driver: &str,
    relation: &SerialRelation,
) -> bool {
    if relation.order.is_some() {
        let serial = campaign_checkpoint_serial(
            checkpoint,
            relation.left.service.as_deref().unwrap_or(driver),
        );
        return serial_ordered_relation_matches(&serial, relation);
    }
    let left = campaign_join_endpoint_values(checkpoint, driver, &relation.left);
    let right = campaign_join_endpoint_values(checkpoint, driver, &relation.right);
    let matches = left
        .iter()
        .filter(|candidate| {
            right
                .iter()
                .any(|value| json_relation_matches(candidate, value, relation.operator))
        })
        .cloned()
        .collect();
    serial_match_requirements_met(left, matches, relation.quantifier, relation.occurs.as_ref())
}

fn serial_ordered_relation_matches(serial: &[u8], relation: &SerialRelation) -> bool {
    let (left, matches) = serial_ordered_relation_values(serial, relation);
    serial_match_requirements_met(left, matches, relation.quantifier, relation.occurs.as_ref())
}

fn serial_ordered_relation_values(
    serial: &[u8],
    relation: &SerialRelation,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let Some(order) = relation.order else {
        return (Vec::new(), Vec::new());
    };
    let left = serial_json_endpoint_values_with_positions(serial, &relation.left);
    let right = serial_json_endpoint_values_with_positions(serial, &relation.right);
    let matches = left
        .iter()
        .filter(|(candidate, candidate_position)| {
            right.iter().any(|(value, value_position)| {
                json_relation_matches(candidate, value, relation.operator)
                    && match order {
                        SerialRelationOrder::Before => candidate_position < value_position,
                        SerialRelationOrder::After => candidate_position > value_position,
                    }
            })
        })
        .map(|(value, _)| value.clone())
        .collect();
    (left.into_iter().map(|(value, _)| value).collect(), matches)
}

fn campaign_serial_path_matches(
    checkpoint: &CampaignCheckpoint,
    driver: &str,
    path: &SerialPath,
) -> bool {
    let serial = campaign_checkpoint_serial(checkpoint, path.service.as_deref().unwrap_or(driver));
    serial_path_matches(&serial, path)
}

fn serial_path_matches(serial: &[u8], path: &SerialPath) -> bool {
    let (candidates, matches) = serial_path_values(serial, path);
    serial_match_requirements_met(candidates, matches, path.quantifier, path.occurs.as_ref())
}

fn serial_path_values(
    serial: &[u8],
    path: &SerialPath,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let step_values = path
        .steps
        .iter()
        .map(|step| {
            serial
                .split_inclusive(|byte| *byte == b'\n')
                .enumerate()
                .filter_map(|(position, line)| {
                    serde_json::from_slice(line.strip_suffix(b"\n").unwrap_or(line))
                        .ok()
                        .map(|event| (event, position))
                })
                .filter(|(event, _)| json_predicate_matches(event, step))
                .filter_map(|(event, position)| {
                    json_path_key(&event, &path.pointers).map(|key| (position, key))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let Some((first, later)) = step_values.split_first() else {
        return (Vec::new(), Vec::new());
    };
    let candidates = first.iter().map(|(_, key)| key.clone()).collect::<Vec<_>>();
    let matches = candidates
        .iter()
        .filter(|key| serial_path_key_matches(first, later, key))
        .cloned()
        .collect();
    (candidates, matches)
}

fn serial_path_key_matches(
    first: &[(usize, serde_json::Value)],
    later: &[Vec<(usize, serde_json::Value)>],
    key: &serde_json::Value,
) -> bool {
    first.iter().any(|(first_position, first_key)| {
        if first_key != key {
            return false;
        }
        later
            .iter()
            .try_fold(*first_position, |previous_position, step| {
                step.iter()
                    .find(|(position, value)| *position > previous_position && value == key)
                    .map(|(position, _)| *position)
            })
            .is_some()
    })
}

fn json_path_key(event: &serde_json::Value, pointers: &[String]) -> Option<serde_json::Value> {
    pointers
        .iter()
        .map(|pointer| event.pointer(pointer).cloned())
        .collect::<Option<Vec<_>>>()
        .map(serde_json::Value::Array)
}

fn campaign_serial_evidence_matches(
    checkpoint: &CampaignCheckpoint,
    driver: &str,
    evidence: &SerialEvidence,
) -> bool {
    if !evidence.all.is_empty() {
        evidence
            .all
            .iter()
            .all(|child| campaign_serial_evidence_matches(checkpoint, driver, child))
    } else if !evidence.any.is_empty() {
        evidence
            .any
            .iter()
            .any(|child| campaign_serial_evidence_matches(checkpoint, driver, child))
    } else if !evidence.none.is_empty() {
        evidence
            .none
            .iter()
            .all(|child| !campaign_serial_evidence_matches(checkpoint, driver, child))
    } else if let Some(guard) = &evidence.guard {
        campaign_serial_guard_matches(checkpoint, driver, guard)
    } else if let Some(correlation) = &evidence.correlation {
        campaign_serial_correlation_matches(checkpoint, driver, correlation)
    } else if let Some(join) = &evidence.join {
        campaign_serial_join_matches(checkpoint, driver, join)
    } else if let Some(relation) = &evidence.relation {
        campaign_serial_relation_matches(checkpoint, driver, relation)
    } else if let Some(path) = &evidence.path {
        campaign_serial_path_matches(checkpoint, driver, path)
    } else {
        false
    }
}

fn campaign_join_endpoint_values(
    checkpoint: &CampaignCheckpoint,
    driver: &str,
    endpoint: &JsonCorrelationEndpoint,
) -> Vec<serde_json::Value> {
    serial_json_endpoint_values(
        &campaign_checkpoint_serial(checkpoint, endpoint.service.as_deref().unwrap_or(driver)),
        endpoint,
    )
}

fn campaign_checkpoint_serial(checkpoint: &CampaignCheckpoint, driver: &str) -> Vec<u8> {
    checkpoint
        .scheduler
        .get(driver)
        .into_iter()
        .flat_map(|service| service.serial_contents.iter())
        .flatten()
        .copied()
        .collect()
}

fn campaign_checkpoint_markers(
    checkpoint: &CampaignCheckpoint,
) -> std::collections::BTreeSet<String> {
    let mut markers = std::collections::BTreeSet::new();
    for service in checkpoint.scheduler.values() {
        for serial in &service.serial_contents {
            for line in String::from_utf8_lossy(serial).lines() {
                if let Some(marker) = line.trim().strip_prefix("THES:M:") {
                    markers.insert(marker.to_owned());
                }
            }
        }
    }
    markers
}

#[derive(Clone, Debug)]
struct CampaignGuidanceObservation {
    operations: Vec<usize>,
    novel_markers: usize,
    novel_state: bool,
    failed: bool,
}

/// Stable state evidence that is meaningful to a topology campaign. It
/// deliberately excludes serial output and applied-fault names: those are
/// independent evidence streams and would make every candidate appear novel.
#[derive(Deserialize, Serialize)]
struct CampaignTopologyState {
    #[serde(default)]
    storage_sha256: BTreeMap<String, String>,
    #[serde(default)]
    network_traffic: BTreeMap<String, NetworkTraffic>,
    #[serde(default)]
    virtual_time_ns: Option<Vec<u64>>,
}

fn campaign_schedules(campaign: &CampaignPlan) -> Vec<CampaignSchedule> {
    let mut schedules = Vec::new();
    for history in campaign_operation_histories(campaign) {
        schedules.push(CampaignSchedule {
            operations: history.clone(),
            faults: Vec::new(),
        });
        if schedules.len() == MAX_CAMPAIGN_CANDIDATES {
            return schedules;
        }
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
            if schedules.len() == MAX_CAMPAIGN_CANDIDATES {
                return schedules;
            }
        }
    }
    schedules
}

/// Enumerate every ordered operation history, including repetitions, in stable
/// breadth-first order. A manifest controls the depth explicitly; the global
/// candidate cap remains the final guard for wide workloads and fault products.
fn campaign_operation_histories(campaign: &CampaignPlan) -> Vec<Vec<usize>> {
    ordered_operation_histories(
        campaign.operations.len(),
        usize::from(campaign.max_operations_per_run),
        |history, operation| campaign_operation_is_ready(campaign, history, operation),
    )
}

fn campaign_operation_is_ready(
    campaign: &CampaignPlan,
    history: &[usize],
    operation: usize,
) -> bool {
    let candidate = &campaign.operations[operation];
    let candidate_stage = candidate.stage.as_ref().and_then(|stage| {
        campaign
            .stages
            .iter()
            .position(|declared| declared == stage)
    });
    let stages_are_ordered = history.iter().all(|prior| {
        let prior_stage = campaign.operations[*prior]
            .stage
            .as_ref()
            .and_then(|stage| {
                campaign
                    .stages
                    .iter()
                    .position(|declared| declared == stage)
            });
        prior_stage.is_none()
            || candidate_stage.is_none_or(|candidate| prior_stage <= Some(candidate))
    });
    stages_are_ordered
        && candidate.requires.iter().all(|requirement| {
            history
                .iter()
                .any(|prior| campaign.operations[*prior].name == requirement.as_str())
        })
        && candidate.excludes.iter().all(|exclusion| {
            history
                .iter()
                .all(|prior| campaign.operations[*prior].name != exclusion.as_str())
        })
        && candidate.max_uses.map_or(true, |maximum| {
            history.iter().filter(|prior| **prior == operation).count() < usize::from(maximum)
        })
}

fn ordered_operation_histories<F>(
    operation_count: usize,
    maximum_depth: usize,
    mut operation_is_ready: F,
) -> Vec<Vec<usize>>
where
    F: FnMut(&[usize], usize) -> bool,
{
    let mut histories = Vec::new();
    let mut frontier = vec![Vec::new()];
    for _ in 0..maximum_depth {
        let mut next = Vec::new();
        for prefix in frontier {
            for operation in 0..operation_count {
                if !operation_is_ready(&prefix, operation) {
                    continue;
                }
                let mut history = prefix.clone();
                history.push(operation);
                histories.push(history.clone());
                next.push(history);
                if histories.len() == MAX_CAMPAIGN_CANDIDATES {
                    return histories;
                }
            }
        }
        frontier = next;
    }
    histories
}

fn recorded_campaign_schedules(
    campaign: &CampaignPlan,
    recorded: &RecordedCampaignResult,
) -> Result<Vec<CampaignSchedule>, String> {
    if recorded.runs.is_empty() {
        return Err("recorded campaign has no schedules".to_owned());
    }
    recorded
        .runs
        .iter()
        .map(|run| {
            let operations = run
                .operations
                .iter()
                .map(|name| {
                    campaign
                        .operations
                        .iter()
                        .position(|operation| operation.name == *name)
                        .ok_or_else(|| {
                            format!("recorded campaign operation is not declared: {name}")
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let names = if run.faults.is_empty() {
                run.fault.iter().cloned().collect::<Vec<_>>()
            } else {
                run.faults.clone()
            };
            let faults = names
                .iter()
                .map(|name| {
                    let matches = campaign
                        .faults
                        .iter()
                        .enumerate()
                        .filter_map(|(index, fault)| {
                            (campaign_fault_name(fault) == *name).then_some(index)
                        })
                        .collect::<Vec<_>>();
                    match matches.as_slice() {
                        [index] => Ok(*index),
                        [] => Err(format!("recorded campaign fault is not declared: {name}")),
                        _ => Err(format!("recorded campaign fault is ambiguous: {name}")),
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CampaignSchedule { operations, faults })
        })
        .collect()
}

fn campaign_replay_mismatches(expected: &RecordedCampaignRun, actual: &CampaignRun) -> Vec<String> {
    let mut mismatches = Vec::new();
    if expected.operations != actual.operations {
        mismatches.push("operations".to_owned());
    }
    let expected_faults = if expected.faults.is_empty() {
        expected.fault.iter().cloned().collect::<Vec<_>>()
    } else {
        expected.faults.clone()
    };
    if expected_faults != actual.faults {
        mismatches.push("faults".to_owned());
    }
    if !expected.actions.is_empty() && expected.actions != actual.actions {
        mismatches.push("actions".to_owned());
    }
    if !expected.selection.is_empty() && expected.selection != actual.selection {
        mismatches.push("selection".to_owned());
    }
    if !expected.program_counters.is_empty() && expected.program_counters != actual.program_counters
    {
        mismatches.push("checkpoint program counters".to_owned());
    }
    if !expected.novelty.is_empty() && expected.novelty != actual.novelty {
        mismatches.push("marker coverage".to_owned());
    }
    if !expected.state_sha256.is_empty()
        && (expected.state_sha256 != actual.state_sha256
            || expected.state_novel != actual.state_novel)
    {
        mismatches.push("topology-state coverage".to_owned());
    }
    if !expected.status.is_empty() && expected.status != actual.status {
        mismatches.push("status".to_owned());
    }
    mismatches
}

/// Choose the next leaf from observed marker and topology-state coverage
/// after first seeding every one-operation history without faults. An
/// observation only guides schedules that extend the exact operation history
/// which produced it; fault variants remain separate leaves and all ties fall
/// back to the stable breadth-first candidate order.
fn select_campaign_schedule(
    schedules: &[CampaignSchedule],
    pending: &[usize],
    observations: &[CampaignGuidanceObservation],
) -> (usize, String) {
    if let Some((pending_index, _)) = pending.iter().enumerate().find(|(_, schedule_index)| {
        let candidate = &schedules[**schedule_index];
        candidate.operations.len() == 1
            && candidate.faults.is_empty()
            && !observations
                .iter()
                .any(|observation| observation.operations == candidate.operations)
    }) {
        return (
            pending_index,
            "canonical breadth-first operation seed".to_owned(),
        );
    }
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
            // Markers are the campaign's user-visible coverage signal. The
            // final drive/network/clock state catches meaningful divergence
            // even when the guest emits the same markers. A failed leaf is
            // also valuable, but neither signal is proof on its own.
            let signal = observation.novel_markers.saturating_mul(1_000)
                + usize::from(observation.novel_state).saturating_mul(250)
                + usize::from(observation.failed).saturating_mul(100);
            let candidate_score = signal.saturating_mul(256) + observation.operations.len();
            if candidate_score > score {
                score = candidate_score;
                reason = Some(observation);
            }
        }
        if score > selected_score {
            selected = pending_index;
            selected_score = score;
            selected_reason =
                campaign_guidance_reason(reason.expect("nonzero guidance has a reason"));
        }
    }
    (selected, selected_reason)
}

fn campaign_guidance_reason(observation: &CampaignGuidanceObservation) -> String {
    let mut signals = Vec::new();
    if observation.novel_markers > 0 {
        signals.push(format!("{} new marker(s)", observation.novel_markers));
    }
    if observation.novel_state {
        signals.push("new topology state".to_owned());
    }
    if observation.failed {
        signals.push("failure".to_owned());
    }
    format!(
        "extends {}-operation prefix with {}",
        observation.operations.len(),
        signals.join(" and ")
    )
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

fn campaign_checkpoint_program_counters(
    checkpoint: &CampaignCheckpoint,
) -> BTreeMap<String, Vec<String>> {
    checkpoint
        .scheduler
        .iter()
        .map(|(service, state)| {
            (
                service.clone(),
                state
                    .program_counters
                    .iter()
                    .map(|pc| format!("{pc:#x}"))
                    .collect(),
            )
        })
        .collect()
}

fn campaign_topology_state_sha256(
    run: &Path,
    program_counters: &BTreeMap<String, Vec<String>>,
) -> Result<String, String> {
    let mut states = BTreeMap::new();
    let services = fs::read_dir(run.join("services")).map_err(|error| error.to_string())?;
    for service in services {
        let service = service.map_err(|error| error.to_string())?;
        let name = service.file_name().to_string_lossy().into_owned();
        let path = service.path().join("result.json");
        let state: CampaignTopologyState = serde_json::from_slice(
            &fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?,
        )
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
        states.insert(name, state);
    }
    let encoded = serde_json::to_vec(&(states, program_counters))
        .map_err(|error| format!("cannot encode campaign topology state: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
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
                    property_matches_in_run(
                        property,
                        &output.join("runs").join(format!("{:03}", run.index)),
                    )
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
                    "{} of {} retained timelines satisfied {}",
                    found,
                    runs.len(),
                    campaign_property_description(property)
                ),
            })
        })
        .collect()
}

fn campaign_serial_matches_property(
    run: &Path,
    service: &str,
    property: &CampaignProperty,
) -> bool {
    let serial = campaign_serial_contents(run, service);
    serial_matches_property(&serial, property)
}

fn campaign_serial_guard_matches_property(
    run: &Path,
    property: &CampaignProperty,
    guard: &OperationSerialGuard,
) -> bool {
    campaign_property_services(
        run,
        guard.service.as_deref().or(property.service.as_deref()),
    )
    .into_iter()
    .any(|service| {
        serial_matches_nested_predicate(&campaign_serial_contents(run, &service), &guard.predicate)
    })
}

fn serial_correlation_matches_property(
    run: &Path,
    property: &CampaignProperty,
    correlation: &SerialCorrelation,
) -> bool {
    let capture_services = campaign_property_services(
        run,
        correlation
            .capture
            .service
            .as_deref()
            .or(property.service.as_deref()),
    );
    let equals_services = campaign_property_services(
        run,
        correlation
            .equals
            .service
            .as_deref()
            .or(property.service.as_deref()),
    );
    let captures = capture_services
        .iter()
        .flat_map(|service| {
            serial_json_endpoint_values(
                &campaign_serial_contents(run, service),
                &correlation.capture,
            )
        })
        .collect::<Vec<_>>();
    !captures.is_empty()
        && equals_services.iter().any(|service| {
            serial_json_endpoint_values(
                &campaign_serial_contents(run, service),
                &correlation.equals,
            )
            .into_iter()
            .any(|value| captures.iter().any(|capture| capture == &value))
        })
}

fn serial_join_matches_property(
    run: &Path,
    property: &CampaignProperty,
    join: &SerialJoin,
) -> bool {
    let Some((first, rest)) = join.endpoints.split_first() else {
        return false;
    };
    let candidates = serial_join_endpoint_values(run, property, first);
    let peer_values = rest
        .iter()
        .map(|endpoint| serial_join_endpoint_values(run, property, endpoint))
        .collect::<Vec<_>>();
    let matches = candidates
        .iter()
        .filter(|candidate| {
            peer_values
                .iter()
                .all(|values| values.iter().any(|value| value == *candidate))
        })
        .cloned()
        .collect();
    serial_match_requirements_met(candidates, matches, join.quantifier, join.occurs.as_ref())
}

fn serial_relation_matches_property(
    run: &Path,
    property: &CampaignProperty,
    relation: &SerialRelation,
) -> bool {
    if relation.order.is_some() {
        let mut left = Vec::new();
        let mut matches = Vec::new();
        for service in campaign_property_services(
            run,
            relation
                .left
                .service
                .as_deref()
                .or(property.service.as_deref()),
        ) {
            let (service_left, service_matches) =
                serial_ordered_relation_values(&campaign_serial_contents(run, &service), relation);
            left.extend(service_left);
            matches.extend(service_matches);
        }
        return serial_match_requirements_met(
            left,
            matches,
            relation.quantifier,
            relation.occurs.as_ref(),
        );
    }
    let left = serial_join_endpoint_values(run, property, &relation.left);
    let right = serial_join_endpoint_values(run, property, &relation.right);
    let matches = left
        .iter()
        .filter(|candidate| {
            right
                .iter()
                .any(|value| json_relation_matches(candidate, value, relation.operator))
        })
        .cloned()
        .collect();
    serial_match_requirements_met(left, matches, relation.quantifier, relation.occurs.as_ref())
}

fn serial_path_matches_property(
    run: &Path,
    property: &CampaignProperty,
    path: &SerialPath,
) -> bool {
    let mut candidates = Vec::new();
    let mut matches = Vec::new();
    for service in
        campaign_property_services(run, path.service.as_deref().or(property.service.as_deref()))
    {
        let (service_candidates, service_matches) =
            serial_path_values(&campaign_serial_contents(run, &service), path);
        candidates.extend(service_candidates);
        matches.extend(service_matches);
    }
    serial_match_requirements_met(candidates, matches, path.quantifier, path.occurs.as_ref())
}

fn serial_evidence_matches_property(
    run: &Path,
    property: &CampaignProperty,
    evidence: &SerialEvidence,
) -> bool {
    if !evidence.all.is_empty() {
        evidence
            .all
            .iter()
            .all(|child| serial_evidence_matches_property(run, property, child))
    } else if !evidence.any.is_empty() {
        evidence
            .any
            .iter()
            .any(|child| serial_evidence_matches_property(run, property, child))
    } else if !evidence.none.is_empty() {
        evidence
            .none
            .iter()
            .all(|child| !serial_evidence_matches_property(run, property, child))
    } else if let Some(guard) = &evidence.guard {
        campaign_serial_guard_matches_property(run, property, guard)
    } else if let Some(correlation) = &evidence.correlation {
        serial_correlation_matches_property(run, property, correlation)
    } else if let Some(join) = &evidence.join {
        serial_join_matches_property(run, property, join)
    } else if let Some(relation) = &evidence.relation {
        serial_relation_matches_property(run, property, relation)
    } else if let Some(path) = &evidence.path {
        serial_path_matches_property(run, property, path)
    } else {
        false
    }
}

fn serial_join_endpoint_values(
    run: &Path,
    property: &CampaignProperty,
    endpoint: &JsonCorrelationEndpoint,
) -> Vec<serde_json::Value> {
    campaign_property_services(
        run,
        endpoint.service.as_deref().or(property.service.as_deref()),
    )
    .into_iter()
    .flat_map(|service| {
        serial_json_endpoint_values(&campaign_serial_contents(run, &service), endpoint)
    })
    .collect()
}

fn serial_json_endpoint_values(
    serial: &[u8],
    endpoint: &JsonCorrelationEndpoint,
) -> Vec<serde_json::Value> {
    serial_json_endpoint_values_with_positions(serial, endpoint)
        .into_iter()
        .map(|(value, _)| value)
        .collect()
}

fn serial_json_endpoint_values_with_positions(
    serial: &[u8],
    endpoint: &JsonCorrelationEndpoint,
) -> Vec<(serde_json::Value, usize)> {
    serial
        .split_inclusive(|byte| *byte == b'\n')
        .enumerate()
        .filter_map(|(position, line)| {
            serde_json::from_slice(line.strip_suffix(b"\n").unwrap_or(line))
                .ok()
                .map(|event| (event, position))
        })
        .filter(|(event, _)| json_predicate_matches(event, &endpoint.json))
        .filter_map(|(event, position)| {
            endpoint_pointers(endpoint)
                .iter()
                .map(|pointer| event.pointer(pointer).cloned())
                .collect::<Option<Vec<_>>>()
                .map(serde_json::Value::Array)
                .map(|value| (value, position))
        })
        .collect()
}

fn endpoint_pointers(endpoint: &JsonCorrelationEndpoint) -> Vec<&str> {
    if endpoint.pointers.is_empty() {
        endpoint.pointer.as_deref().into_iter().collect()
    } else {
        endpoint.pointers.iter().map(String::as_str).collect()
    }
}

fn campaign_serial_contents(run: &Path, service: &str) -> Vec<u8> {
    let mut logs = fs::read_dir(run.join("services").join(service))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == "serial.log" || (name.starts_with("serial-") && name.ends_with(".log"))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    logs.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| {
                name.strip_prefix("serial-")
                    .and_then(|suffix| suffix.strip_suffix(".log"))
                    .and_then(|index| index.parse::<usize>().ok())
            })
            .map(|index| index + 1)
            .unwrap_or(0)
    });
    let serial = logs
        .into_iter()
        .flat_map(|path| fs::read(path).unwrap_or_default())
        .collect::<Vec<_>>();
    serial
}

fn serial_matches_property(serial: &[u8], property: &CampaignProperty) -> bool {
    serial_matches_predicate(
        serial,
        property.contains.as_deref().unwrap_or_default(),
        &property.contains_all,
        &property.contains_any,
        &property.contains_none,
    ) && property
        .predicate
        .as_ref()
        .map(|predicate| serial_matches_nested_predicate(serial, predicate))
        .unwrap_or(true)
}

fn serial_matches_predicate(
    serial: &[u8],
    contains: &str,
    contains_all: &[String],
    contains_any: &[String],
    contains_none: &[String],
) -> bool {
    (contains.is_empty() || serial_contains(serial, contains))
        && contains_all
            .iter()
            .all(|needle| serial_contains(serial, needle))
        && (contains_any.is_empty()
            || contains_any
                .iter()
                .any(|needle| serial_contains(serial, needle)))
        && contains_none
            .iter()
            .all(|needle| !serial_contains(serial, needle))
}

fn serial_matches_nested_predicate(serial: &[u8], predicate: &SerialPredicate) -> bool {
    predicate
        .contains
        .as_ref()
        .map(|needle| serial_contains(serial, needle))
        .unwrap_or(true)
        && predicate
            .matches
            .as_ref()
            .map(|expression| {
                Regex::new(expression)
                    .map(|expression| expression.is_match(serial))
                    .unwrap_or(false)
            })
            .unwrap_or(true)
        && predicate
            .json
            .as_ref()
            .map(|predicate| serial_matches_json_predicate(serial, predicate))
            .unwrap_or(true)
        && predicate
            .all
            .iter()
            .all(|child| serial_matches_nested_predicate(serial, child))
        && (predicate.any.is_empty()
            || predicate
                .any
                .iter()
                .any(|child| serial_matches_nested_predicate(serial, child)))
        && predicate
            .none
            .iter()
            .all(|child| !serial_matches_nested_predicate(serial, child))
        && (predicate.sequence.is_empty() || serial_matches_sequence(serial, &predicate.sequence))
        && predicate
            .occurs
            .as_ref()
            .map(|occurs| serial_occurrence_matches(serial, occurs))
            .unwrap_or(true)
}

fn serial_occurrence_matches(serial: &[u8], occurs: &SerialOccurrence) -> bool {
    let count = serial_predicate_occurrences(serial, &occurs.predicate);
    occurs.exactly.is_none_or(|exactly| count == exactly)
        && occurs.at_least.is_none_or(|at_least| count >= at_least)
        && occurs.at_most.is_none_or(|at_most| count <= at_most)
}

fn serial_predicate_occurrences(serial: &[u8], predicate: &SerialPredicate) -> u64 {
    if let Some(needle) = &predicate.contains {
        return serial_non_overlapping_count(serial, needle.as_bytes());
    }
    if let Some(expression) = &predicate.matches {
        return Regex::new(expression)
            .map(|expression| expression.find_iter(serial).count() as u64)
            .unwrap_or(0);
    }
    predicate
        .json
        .as_ref()
        .map(|predicate| {
            serial
                .split_inclusive(|byte| *byte == b'\n')
                .filter(|line| {
                    serial_json_event_matches(line.strip_suffix(b"\n").unwrap_or(line), predicate)
                })
                .count() as u64
        })
        .unwrap_or(0)
}

fn serial_non_overlapping_count(serial: &[u8], needle: &[u8]) -> u64 {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut offset = 0;
    while let Some(start) = serial[offset..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        count += 1;
        offset += start + needle.len();
    }
    count
}

fn serial_matches_sequence(serial: &[u8], sequence: &[SerialPredicate]) -> bool {
    serial_sequence_matches_from(serial, sequence, 0, 0, BTreeMap::new())
}

fn serial_sequence_matches_from(
    serial: &[u8],
    sequence: &[SerialPredicate],
    index: usize,
    offset: usize,
    captures: BTreeMap<String, serde_json::Value>,
) -> bool {
    let Some(predicate) = sequence.get(index) else {
        return true;
    };
    serial_sequence_item_match_ends(&serial[offset..], predicate, &captures)
        .into_iter()
        .any(|(end, captures)| {
            serial_sequence_matches_from(serial, sequence, index + 1, offset + end, captures)
        })
}

fn serial_sequence_item_match_ends(
    serial: &[u8],
    predicate: &SerialPredicate,
    captures: &BTreeMap<String, serde_json::Value>,
) -> Vec<(usize, BTreeMap<String, serde_json::Value>)> {
    if let Some(needle) = &predicate.contains {
        let needle = needle.as_bytes();
        return serial
            .windows(needle.len())
            .enumerate()
            .filter_map(|(start, window)| {
                (window == needle).then_some((start + needle.len(), captures.clone()))
            })
            .collect();
    }
    if let Some(expression) = &predicate.matches {
        return Regex::new(expression)
            .ok()
            .map(|expression| {
                expression
                    .find_iter(serial)
                    .map(|matched| (matched.end(), captures.clone()))
                    .collect()
            })
            .unwrap_or_default();
    }
    predicate
        .json
        .as_ref()
        .map(|predicate| {
            serial_json_predicate_match_ends_with_captures(serial, predicate, captures)
        })
        .unwrap_or_default()
}

fn serial_matches_json_predicate(serial: &[u8], predicate: &JsonPredicate) -> bool {
    serial_json_predicate_match_end(serial, predicate).is_some()
}

fn serial_json_predicate_match_end(serial: &[u8], predicate: &JsonPredicate) -> Option<usize> {
    serial_json_predicate_match_end_with_captures(serial, predicate, &mut BTreeMap::new())
}

fn serial_json_predicate_match_end_with_captures(
    serial: &[u8],
    predicate: &JsonPredicate,
    captures: &mut BTreeMap<String, serde_json::Value>,
) -> Option<usize> {
    let mut start = 0;
    for line in serial.split_inclusive(|byte| *byte == b'\n') {
        let end = start + line.len();
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let mut candidate_captures = captures.clone();
        let matches = serde_json::from_slice::<serde_json::Value>(line)
            .ok()
            .is_some_and(|event| {
                json_predicate_matches_with_captures(&event, predicate, &mut candidate_captures)
            });
        if matches {
            *captures = candidate_captures;
            return Some(end);
        }
        start = end;
    }
    None
}

fn serial_json_predicate_match_ends_with_captures(
    serial: &[u8],
    predicate: &JsonPredicate,
    captures: &BTreeMap<String, serde_json::Value>,
) -> Vec<(usize, BTreeMap<String, serde_json::Value>)> {
    let mut matches = Vec::new();
    let mut start = 0;
    for line in serial.split_inclusive(|byte| *byte == b'\n') {
        let end = start + line.len();
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let mut candidate_captures = captures.clone();
        if serde_json::from_slice::<serde_json::Value>(line)
            .ok()
            .is_some_and(|event| {
                json_predicate_matches_with_captures(&event, predicate, &mut candidate_captures)
            })
        {
            matches.push((end, candidate_captures));
        }
        start = end;
    }
    matches
}

fn serial_json_event_matches(line: &[u8], predicate: &JsonPredicate) -> bool {
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()
        .is_some_and(|event| json_predicate_matches(&event, predicate))
}

fn json_predicate_matches(event: &serde_json::Value, predicate: &JsonPredicate) -> bool {
    json_predicate_matches_with_captures(event, predicate, &mut BTreeMap::new())
}

fn json_predicate_matches_with_captures(
    event: &serde_json::Value,
    predicate: &JsonPredicate,
    captures: &mut BTreeMap<String, serde_json::Value>,
) -> bool {
    predicate
        .fields
        .iter()
        .all(|(pointer, expected)| event.pointer(pointer) == Some(expected))
        && predicate
            .where_
            .iter()
            .all(|condition| json_condition_matches(event, condition))
        && predicate.arrays.iter().all(|array| {
            let Some(values) = event
                .pointer(&array.pointer)
                .and_then(serde_json::Value::as_array)
            else {
                return false;
            };
            array.any.as_ref().is_none_or(|predicate| {
                values
                    .iter()
                    .any(|value| json_predicate_matches(value, predicate))
            }) && array.all.as_ref().is_none_or(|predicate| {
                values
                    .iter()
                    .all(|value| json_predicate_matches(value, predicate))
            }) && array.none.as_ref().is_none_or(|predicate| {
                values
                    .iter()
                    .all(|value| !json_predicate_matches(value, predicate))
            })
        })
        && predicate
            .equals_capture
            .iter()
            .all(|(pointer, name)| event.pointer(pointer) == captures.get(name))
        && predicate
            .capture
            .values()
            .all(|pointer| event.pointer(pointer).is_some())
        && {
            for (name, pointer) in &predicate.capture {
                captures.insert(
                    name.clone(),
                    event.pointer(pointer).expect("validated above").clone(),
                );
            }
            true
        }
}

fn json_condition_matches(event: &serde_json::Value, condition: &JsonCondition) -> bool {
    let actual = event.pointer(&condition.pointer);
    if let Some(expected) = condition.exists {
        return actual.is_some() == expected;
    }
    if let Some(expected) = &condition.equals {
        return actual == Some(expected);
    }
    if let Some(expression) = &condition.matches {
        return actual
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| {
                Regex::new(expression)
                    .map(|expression| expression.is_match(value.as_bytes()))
                    .unwrap_or(false)
            });
    }
    let Some(actual) = actual.and_then(serde_json::Value::as_f64) else {
        return false;
    };
    if let Some(expected) = condition.greater_than {
        return actual > expected;
    }
    if let Some(expected) = condition.greater_than_or_equal {
        return actual >= expected;
    }
    if let Some(expected) = condition.less_than {
        return actual < expected;
    }
    condition
        .less_than_or_equal
        .is_some_and(|expected| actual <= expected)
}

fn serial_contains(serial: &[u8], needle: &str) -> bool {
    !needle.is_empty()
        && serial
            .windows(needle.len())
            .any(|value| value == needle.as_bytes())
}

fn campaign_property_description(property: &CampaignProperty) -> String {
    let mut clauses = property
        .contains
        .as_ref()
        .map(|contains| vec![format!("contains {contains:?}")])
        .unwrap_or_default();
    if !property.contains_all.is_empty() {
        clauses.push(format!("also contains all {:?}", property.contains_all));
    }
    if !property.contains_any.is_empty() {
        clauses.push(format!("also contains one of {:?}", property.contains_any));
    }
    if !property.contains_none.is_empty() {
        clauses.push(format!("contains none of {:?}", property.contains_none));
    }
    if let Some(predicate) = &property.predicate {
        let description = nested_predicate_description(predicate);
        clauses.push(if clauses.is_empty() {
            description
        } else {
            format!("also satisfies {description}")
        });
    }
    if !property.requires_serial_all.is_empty() {
        clauses.push(format!(
            "also requires all [{}]",
            property
                .requires_serial_all
                .iter()
                .map(serial_guard_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !property.requires_serial_any.is_empty() {
        clauses.push(format!(
            "also requires any [{}]",
            property
                .requires_serial_any
                .iter()
                .map(serial_guard_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !property.excludes_serial_any.is_empty() {
        clauses.push(format!(
            "also excludes any [{}]",
            property
                .excludes_serial_any
                .iter()
                .map(serial_guard_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !property.requires_serial_correlations.is_empty() {
        clauses.push(format!(
            "also requires correlations [{}]",
            property
                .requires_serial_correlations
                .iter()
                .map(serial_correlation_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !property.requires_serial_joins.is_empty() {
        clauses.push(format!(
            "also requires joins [{}]",
            property
                .requires_serial_joins
                .iter()
                .map(serial_join_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(evidence) = &property.requires_serial_evidence {
        clauses.push(format!(
            "also requires serial evidence {}",
            serial_evidence_description(evidence)
        ));
    }
    if let Some(evidence) = &property.excludes_serial_evidence {
        clauses.push(format!(
            "also excludes serial evidence {}",
            serial_evidence_description(evidence)
        ));
    }
    clauses.join("; ")
}

fn serial_evidence_description(evidence: &SerialEvidence) -> String {
    if !evidence.all.is_empty() {
        format!(
            "all [{}]",
            evidence
                .all
                .iter()
                .map(serial_evidence_description)
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else if !evidence.any.is_empty() {
        format!(
            "any [{}]",
            evidence
                .any
                .iter()
                .map(serial_evidence_description)
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else if !evidence.none.is_empty() {
        format!(
            "none [{}]",
            evidence
                .none
                .iter()
                .map(serial_evidence_description)
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else if let Some(guard) = &evidence.guard {
        serial_guard_description(guard)
    } else if let Some(correlation) = &evidence.correlation {
        serial_correlation_description(correlation)
    } else if let Some(join) = &evidence.join {
        serial_join_description(join)
    } else if let Some(relation) = &evidence.relation {
        serial_relation_description(relation)
    } else if let Some(path) = &evidence.path {
        serial_path_description(path)
    } else {
        "invalid expression".to_owned()
    }
}

fn serial_correlation_description(correlation: &SerialCorrelation) -> String {
    format!(
        "{} {} = {} {}",
        correlation
            .capture
            .service
            .as_deref()
            .unwrap_or("property service"),
        endpoint_pointer_description(&correlation.capture),
        correlation
            .equals
            .service
            .as_deref()
            .unwrap_or("property service"),
        endpoint_pointer_description(&correlation.equals),
    )
}

fn serial_join_description(join: &SerialJoin) -> String {
    let endpoints = join
        .endpoints
        .iter()
        .map(|endpoint| {
            format!(
                "{} {}",
                endpoint.service.as_deref().unwrap_or("property service"),
                endpoint_pointer_description(endpoint)
            )
        })
        .collect::<Vec<_>>()
        .join(" = ");
    let description = match join.quantifier {
        SerialJoinQuantifier::Any => endpoints,
        SerialJoinQuantifier::Every => format!("every {endpoints}"),
    };
    join.occurs
        .as_ref()
        .map(|occurs| format!("{description}; {}", serial_match_count_description(occurs)))
        .unwrap_or(description)
}

fn serial_relation_description(relation: &SerialRelation) -> String {
    let operator = match relation.operator {
        JsonRelationOperator::Equals => "=",
        JsonRelationOperator::NotEquals => "!=",
        JsonRelationOperator::GreaterThan => ">",
        JsonRelationOperator::GreaterThanOrEqual => ">=",
        JsonRelationOperator::LessThan => "<",
        JsonRelationOperator::LessThanOrEqual => "<=",
    };
    let description = format!(
        "{} {} {} {} {}",
        relation
            .left
            .service
            .as_deref()
            .unwrap_or("property service"),
        endpoint_pointer_description(&relation.left),
        operator,
        relation
            .right
            .service
            .as_deref()
            .unwrap_or("property service"),
        endpoint_pointer_description(&relation.right)
    );
    let description = match relation.order {
        None => description,
        Some(SerialRelationOrder::Before) => {
            format!("{description}; left event before right event")
        }
        Some(SerialRelationOrder::After) => {
            format!("{description}; left event after right event")
        }
    };
    let description = match relation.quantifier {
        SerialJoinQuantifier::Any => description,
        SerialJoinQuantifier::Every => format!("every {description}"),
    };
    relation
        .occurs
        .as_ref()
        .map(|occurs| format!("{description}; {}", serial_match_count_description(occurs)))
        .unwrap_or(description)
}

fn serial_path_description(path: &SerialPath) -> String {
    let description = format!(
        "{} ordered JSON event steps in {} keyed by {}",
        path.steps.len(),
        path.service.as_deref().unwrap_or("property service"),
        path.pointers.join(" + ")
    );
    let description = match path.quantifier {
        SerialJoinQuantifier::Any => description,
        SerialJoinQuantifier::Every => format!("every {description}"),
    };
    path.occurs
        .as_ref()
        .map(|occurs| format!("{description}; {}", serial_match_count_description(occurs)))
        .unwrap_or(description)
}

fn serial_match_count_description(occurs: &SerialMatchCount) -> String {
    let mut bounds = Vec::new();
    if let Some(exactly) = occurs.exactly {
        bounds.push(format!("exactly {exactly}"));
    }
    if let Some(at_least) = occurs.at_least {
        bounds.push(format!("at least {at_least}"));
    }
    if let Some(at_most) = occurs.at_most {
        bounds.push(format!("at most {at_most}"));
    }
    format!("{} distinct source keys", bounds.join(" and "))
}

fn endpoint_pointer_description(endpoint: &JsonCorrelationEndpoint) -> String {
    endpoint_pointers(endpoint).join(" + ")
}

fn serial_guard_description(guard: &OperationSerialGuard) -> String {
    let predicate = nested_predicate_description(&guard.predicate);
    guard
        .service
        .as_ref()
        .map(|service| format!("{service}: {predicate}"))
        .unwrap_or(predicate)
}

fn nested_predicate_description(predicate: &SerialPredicate) -> String {
    let mut clauses = predicate
        .contains
        .as_ref()
        .map(|contains| vec![format!("contains {contains:?}")])
        .unwrap_or_default();
    if let Some(expression) = &predicate.matches {
        clauses.push(format!("matches {expression:?}"));
    }
    if let Some(json) = &predicate.json {
        clauses.push(format!("JSON event has fields {:?}", json.fields));
        for condition in &json.where_ {
            clauses.push(json_condition_description(condition));
        }
    }
    if !predicate.all.is_empty() {
        clauses.push(format!(
            "all [{}]",
            predicate
                .all
                .iter()
                .map(nested_predicate_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !predicate.any.is_empty() {
        clauses.push(format!(
            "any [{}]",
            predicate
                .any
                .iter()
                .map(nested_predicate_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !predicate.none.is_empty() {
        clauses.push(format!(
            "none [{}]",
            predicate
                .none
                .iter()
                .map(nested_predicate_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !predicate.sequence.is_empty() {
        clauses.push(format!(
            "sequence [{}]",
            predicate
                .sequence
                .iter()
                .map(nested_predicate_description)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(occurs) = &predicate.occurs {
        let bounds = if let Some(exactly) = occurs.exactly {
            format!("exactly {exactly}")
        } else {
            [
                occurs.at_least.map(|value| format!("at least {value}")),
                occurs.at_most.map(|value| format!("at most {value}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" and ")
        };
        clauses.push(format!(
            "occurs {bounds} times [{}]",
            nested_predicate_description(&occurs.predicate)
        ));
    }
    clauses.join("; ")
}

fn json_condition_description(condition: &JsonCondition) -> String {
    if let Some(value) = &condition.equals {
        return format!("JSON {:?} equals {value}", condition.pointer);
    }
    if let Some(expression) = &condition.matches {
        return format!("JSON {:?} matches {expression:?}", condition.pointer);
    }
    if let Some(value) = condition.greater_than {
        return format!("JSON {:?} > {value}", condition.pointer);
    }
    if let Some(value) = condition.greater_than_or_equal {
        return format!("JSON {:?} >= {value}", condition.pointer);
    }
    if let Some(value) = condition.less_than {
        return format!("JSON {:?} < {value}", condition.pointer);
    }
    if let Some(value) = condition.less_than_or_equal {
        return format!("JSON {:?} <= {value}", condition.pointer);
    }
    format!(
        "JSON {:?} {}",
        condition.pointer,
        if condition.exists == Some(true) {
            "exists"
        } else {
            "is absent"
        }
    )
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

fn recorded_campaign_result(plan: &Path) -> Result<Option<RecordedCampaignResult>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let path = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?
        .join("campaign-result.json");
    match fs::read(&path) {
        Ok(result) => serde_json::from_slice(&result)
            .map(Some)
            .map_err(|error| format!("cannot parse {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
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
            let serial = serial_logs
                .iter()
                .flat_map(|path| fs::read(path).unwrap_or_default())
                .collect::<Vec<_>>();
            let predicate_matches = serial_matches_predicate(
                &serial,
                &check.value,
                &check.contains_all,
                &check.contains_any,
                &check.contains_none,
            ) && check
                .predicate
                .as_ref()
                .map(|predicate| serial_matches_nested_predicate(&serial, predicate))
                .unwrap_or(true);
            let passed = match check.kind {
                CheckKind::SerialContains | CheckKind::MarkerSeen => contains,
                CheckKind::SerialNotContains | CheckKind::MarkerNotSeen => !contains,
                CheckKind::SerialPropertyMatches => predicate_matches,
                CheckKind::SerialPropertyDoesNotMatch => !predicate_matches,
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
    fn json_relations_compare_one_or_composite_endpoint_values() {
        let one = serde_json::json!([2]);
        let two = serde_json::json!([3]);
        let tuple = serde_json::json!(["r-17", 2]);

        assert!(json_relation_matches(
            &two,
            &one,
            JsonRelationOperator::GreaterThan
        ));
        assert!(json_relation_matches(
            &one,
            &two,
            JsonRelationOperator::LessThanOrEqual
        ));
        assert!(!json_relation_matches(
            &tuple,
            &one,
            JsonRelationOperator::GreaterThan
        ));
        assert!(json_relation_matches(
            &tuple,
            &tuple,
            JsonRelationOperator::Equals
        ));
        assert!(json_relation_matches(
            &tuple,
            &one,
            JsonRelationOperator::NotEquals
        ));
    }

    #[test]
    fn ordered_json_relations_match_distinct_values_in_one_transcript() {
        let relation: SerialRelation = serde_json::from_value(serde_json::json!({
            "left": {"pointer": "/request_id", "json": {"fields": {"/event": "write"}}},
            "right": {"pointer": "/request_id", "json": {"fields": {"/event": "replicated"}}},
            "operator": "equals",
            "order": "before",
            "quantifier": "every",
            "occurs": {"exactly": 1}
        }))
        .unwrap();

        assert!(serial_ordered_relation_matches(
            b"{\"event\":\"write\",\"request_id\":\"r-17\"}\n{\"event\":\"write\",\"request_id\":\"r-17\"}\n{\"event\":\"replicated\",\"request_id\":\"r-17\"}\n",
            &relation,
        ));
        assert!(!serial_ordered_relation_matches(
            b"{\"event\":\"replicated\",\"request_id\":\"r-17\"}\n{\"event\":\"write\",\"request_id\":\"r-17\"}\n",
            &relation,
        ));
        assert!(!serial_ordered_relation_matches(
            b"{\"event\":\"write\",\"request_id\":\"r-17\"}\n{\"event\":\"replicated\",\"request_id\":\"r-17\"}\n{\"event\":\"write\",\"request_id\":\"r-18\"}\n{\"event\":\"replicated\",\"request_id\":\"r-18\"}\n",
            &relation,
        ));
        let checkpoint = CampaignCheckpoint {
            switches: BTreeMap::new(),
            services: BTreeMap::new(),
            scheduler: BTreeMap::from([(
                "api".to_owned(),
                ServiceSchedulerCheckpoint {
                    serial_contents: vec![
                        b"{\"event\":\"write\",\"request_id\":\"r-17\"}\n{\"event\":\"replicated\",\"request_id\":\"r-17\"}\n"
                            .to_vec(),
                    ],
                    program_counters: Vec::new(),
                    next_fault: 0,
                    paused_until: None,
                    faults: Vec::new(),
                    network_traffic: BTreeMap::new(),
                    network_trace: BTreeMap::new(),
                },
            )]),
            round: 0,
        };
        assert!(campaign_serial_relation_matches(
            &checkpoint,
            "api",
            &relation
        ));
        assert!(serial_relation_description(&relation).contains("left event before right event"));
    }

    #[test]
    fn ordered_json_relations_apply_to_campaign_properties() {
        let run = std::env::temp_dir().join(format!(
            "theseus-ordered-relation-property-{}",
            std::process::id()
        ));
        let api = run.join("services/api");
        fs::create_dir_all(&api).unwrap();
        let property: CampaignProperty = serde_json::from_value(serde_json::json!({
            "name": "ordered_replication",
            "kind": "always",
            "service": "api",
            "requires_serial_evidence": {
                "relation": {
                    "left": {"pointer": "/request_id", "json": {"fields": {"/event": "write"}}},
                    "right": {"pointer": "/request_id", "json": {"fields": {"/event": "replicated"}}},
                    "operator": "equals",
                    "order": "before"
                }
            }
        }))
        .unwrap();

        fs::write(
            api.join("serial.log"),
            "{\"event\":\"write\",\"request_id\":\"r-17\"}\n{\"event\":\"replicated\",\"request_id\":\"r-17\"}\n",
        )
        .unwrap();
        assert!(property_matches_in_run(&property, &run));
        fs::write(
            api.join("serial.log"),
            "{\"event\":\"replicated\",\"request_id\":\"r-17\"}\n{\"event\":\"write\",\"request_id\":\"r-17\"}\n",
        )
        .unwrap();
        assert!(!property_matches_in_run(&property, &run));
        fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn keyed_json_event_paths_require_a_complete_ordered_lifecycle() {
        let path: SerialPath = serde_json::from_value(serde_json::json!({
            "pointers": ["/request_id", "/attempt"],
            "steps": [
                {"fields": {"/event": "write"}},
                {"fields": {"/event": "replicated"}},
                {"fields": {"/event": "committed"}}
            ],
            "quantifier": "every",
            "occurs": {"exactly": 1}
        }))
        .unwrap();
        let complete = b"{\"event\":\"write\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"write\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"replicated\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"committed\",\"request_id\":\"r-17\",\"attempt\":1}\n";
        assert!(serial_path_matches(complete, &path));
        assert!(!serial_path_matches(
            b"{\"event\":\"write\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"committed\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"replicated\",\"request_id\":\"r-17\",\"attempt\":1}\n",
            &path,
        ));
        assert!(!serial_path_matches(
            b"{\"event\":\"write\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"replicated\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"committed\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"write\",\"request_id\":\"r-18\",\"attempt\":1}\n{\"event\":\"replicated\",\"request_id\":\"r-18\",\"attempt\":1}\n{\"event\":\"committed\",\"request_id\":\"r-18\",\"attempt\":1}\n",
            &path,
        ));
        let checkpoint = CampaignCheckpoint {
            switches: BTreeMap::new(),
            services: BTreeMap::new(),
            scheduler: BTreeMap::from([(
                "api".to_owned(),
                ServiceSchedulerCheckpoint {
                    serial_contents: vec![complete.to_vec()],
                    program_counters: Vec::new(),
                    next_fault: 0,
                    paused_until: None,
                    faults: Vec::new(),
                    network_traffic: BTreeMap::new(),
                    network_trace: BTreeMap::new(),
                },
            )]),
            round: 0,
        };
        assert!(campaign_serial_path_matches(&checkpoint, "api", &path));
        assert!(serial_path_description(&path).contains("3 ordered JSON event steps"));
    }

    #[test]
    fn keyed_json_event_paths_apply_to_campaign_properties() {
        let run = std::env::temp_dir().join(format!(
            "theseus-keyed-path-property-{}",
            std::process::id()
        ));
        let api = run.join("services/api");
        fs::create_dir_all(&api).unwrap();
        let property: CampaignProperty = serde_json::from_value(serde_json::json!({
            "name": "complete_write",
            "kind": "always",
            "service": "api",
            "requires_serial_evidence": {
                "path": {
                    "pointers": ["/request_id"],
                    "steps": [
                        {"fields": {"/event": "write"}},
                        {"fields": {"/event": "committed"}}
                    ],
                    "quantifier": "every"
                }
            }
        }))
        .unwrap();

        fs::write(
            api.join("serial.log"),
            "{\"event\":\"write\",\"request_id\":\"r-17\"}\n{\"event\":\"committed\",\"request_id\":\"r-17\"}\n",
        )
        .unwrap();
        assert!(property_matches_in_run(&property, &run));
        fs::write(
            api.join("serial.log"),
            "{\"event\":\"write\",\"request_id\":\"r-17\"}\n{\"event\":\"committed\",\"request_id\":\"r-18\"}\n",
        )
        .unwrap();
        assert!(!property_matches_in_run(&property, &run));
        fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn nested_serial_properties_require_one_transcript() {
        let run =
            std::env::temp_dir().join(format!("theseus-compound-property-{}", std::process::id()));
        let api = run.join("services/api");
        let worker = run.join("services/worker");
        let auditor = run.join("services/auditor");
        fs::create_dir_all(&api).unwrap();
        fs::create_dir_all(&worker).unwrap();
        fs::create_dir_all(&auditor).unwrap();
        fs::write(api.join("serial.log"), "THES:ASSERT:write:pass\n").unwrap();
        fs::write(worker.join("serial.log"), "THES:M:written\n").unwrap();
        let property = CampaignProperty {
            name: "durable_write".to_owned(),
            kind: PropertyKind::Always,
            contains: None,
            contains_all: Vec::new(),
            contains_any: Vec::new(),
            contains_none: Vec::new(),
            predicate: Some(SerialPredicate {
                contains: None,
                matches: None,
                json: None,
                all: vec![
                    SerialPredicate {
                        contains: Some("THES:ASSERT:write:pass".to_owned()),
                        matches: None,
                        json: None,
                        all: Vec::new(),
                        any: Vec::new(),
                        none: Vec::new(),
                        sequence: Vec::new(),
                        occurs: None,
                    },
                    SerialPredicate {
                        contains: Some("THES:M:written".to_owned()),
                        matches: None,
                        json: None,
                        all: Vec::new(),
                        any: Vec::new(),
                        none: Vec::new(),
                        sequence: Vec::new(),
                        occurs: None,
                    },
                ],
                any: vec![
                    SerialPredicate {
                        contains: Some("THES:CHECKPOINT:write".to_owned()),
                        matches: None,
                        json: None,
                        all: Vec::new(),
                        any: Vec::new(),
                        none: Vec::new(),
                        sequence: Vec::new(),
                        occurs: None,
                    },
                    SerialPredicate {
                        contains: None,
                        matches: Some("THES:M:write_[a-z]+".to_owned()),
                        json: None,
                        all: Vec::new(),
                        any: Vec::new(),
                        none: Vec::new(),
                        sequence: Vec::new(),
                        occurs: None,
                    },
                ],
                none: vec![SerialPredicate {
                    contains: Some("THES:ASSERT:panic".to_owned()),
                    matches: None,
                    json: None,
                    all: Vec::new(),
                    any: Vec::new(),
                    none: Vec::new(),
                    sequence: Vec::new(),
                    occurs: None,
                }],
                sequence: Vec::new(),
                occurs: None,
            }),
            requires_serial_all: Vec::new(),
            requires_serial_any: Vec::new(),
            excludes_serial_any: Vec::new(),
            requires_serial_correlations: Vec::new(),
            requires_serial_joins: Vec::new(),
            requires_serial_evidence: None,
            excludes_serial_evidence: None,
            service: None,
        };

        assert!(!property_matches_in_run(&property, &run));
        fs::write(
            api.join("serial.log"),
            "THES:ASSERT:write:pass\nTHES:M:written\nTHES:CHECKPOINT:write\n",
        )
        .unwrap();
        assert!(property_matches_in_run(&property, &run));
        let mut joined = property.clone();
        joined.requires_serial_all = vec![OperationSerialGuard {
            service: Some("worker".to_owned()),
            predicate: serde_json::from_value(serde_json::json!({
                "contains": "THES:M:written"
            }))
            .unwrap(),
        }];
        assert!(property_matches_in_run(&joined, &run));
        fs::write(worker.join("serial.log"), "THES:M:missing\n").unwrap();
        assert!(!property_matches_in_run(&joined, &run));
        fs::write(worker.join("serial.log"), "THES:M:written\n").unwrap();
        joined.requires_serial_any = vec![OperationSerialGuard {
            service: Some("worker".to_owned()),
            predicate: serde_json::from_value(serde_json::json!({
                "contains": "THES:M:written"
            }))
            .unwrap(),
        }];
        joined.excludes_serial_any = vec![OperationSerialGuard {
            service: Some("worker".to_owned()),
            predicate: serde_json::from_value(serde_json::json!({
                "contains": "THES:ASSERT:panic"
            }))
            .unwrap(),
        }];
        assert!(property_matches_in_run(&joined, &run));
        fs::write(
            worker.join("serial.log"),
            "THES:M:written\nTHES:ASSERT:panic\n",
        )
        .unwrap();
        assert!(!property_matches_in_run(&joined, &run));
        fs::write(worker.join("serial.log"), "THES:M:written\n").unwrap();
        fs::write(
            api.join("serial.log"),
            "THES:ASSERT:write:pass\nTHES:M:written\nTHES:M:write_complete\n",
        )
        .unwrap();
        assert!(property_matches_in_run(&property, &run));
        fs::write(
            api.join("serial.log"),
            "THES:ASSERT:write:pass\nTHES:M:written\nTHES:M:write_complete\nTHES:ASSERT:panic\n",
        )
        .unwrap();
        assert!(!property_matches_in_run(&property, &run));

        fs::write(
            api.join("serial.log"),
            "THES:ASSERT:write:pass\nTHES:M:written\nTHES:CHECKPOINT:write\n{\"event\":\"write\",\"request_id\":\"r-17\",\"attempt\":1}\n",
        )
        .unwrap();
        fs::write(
            worker.join("serial.log"),
            "THES:M:written\n{\"event\":\"replicated\",\"request_id\":\"r-17\",\"attempt\":1}\n",
        )
        .unwrap();
        joined.requires_serial_correlations = vec![serde_json::from_value(serde_json::json!({
            "capture": {
                "service": "api",
                "pointer": "/request_id",
                "json": {"fields": {"/event": "write"}}
            },
            "equals": {
                "service": "worker",
                "pointer": "/request_id",
                "json": {"fields": {"/event": "replicated"}}
            }
        }))
        .unwrap()];
        assert!(property_matches_in_run(&joined, &run));
        fs::write(
            worker.join("serial.log"),
            "THES:M:written\n{\"event\":\"replicated\",\"request_id\":\"r-18\"}\n",
        )
        .unwrap();
        assert!(!property_matches_in_run(&joined, &run));
        fs::write(
            worker.join("serial.log"),
            "THES:M:written\n{\"event\":\"replicated\",\"request_id\":\"r-17\",\"attempt\":1}\n",
        )
        .unwrap();
        fs::write(
            auditor.join("serial.log"),
            "{\"event\":\"audit\",\"request_id\":\"r-18\",\"attempt\":1}\n",
        )
        .unwrap();
        joined.requires_serial_joins = vec![serde_json::from_value(serde_json::json!({
            "endpoints": [
                {"service": "api", "pointers": ["/request_id", "/attempt"], "json": {"fields": {"/event": "write"}}},
                {"service": "worker", "pointers": ["/request_id", "/attempt"], "json": {"fields": {"/event": "replicated"}}},
                {"service": "auditor", "pointers": ["/request_id", "/attempt"], "json": {"fields": {"/event": "audit"}}}
            ]
        }))
        .unwrap()];
        assert!(!property_matches_in_run(&joined, &run));
        fs::write(
            auditor.join("serial.log"),
            "{\"event\":\"audit\",\"request_id\":\"r-17\",\"attempt\":2}\n",
        )
        .unwrap();
        assert!(!property_matches_in_run(&joined, &run));
        fs::write(
            auditor.join("serial.log"),
            "{\"event\":\"audit\",\"request_id\":\"r-17\",\"attempt\":1}\n",
        )
        .unwrap();
        assert!(property_matches_in_run(&joined, &run));
        joined.requires_serial_evidence = Some(
            serde_json::from_value(serde_json::json!({
                "all": [
                    {"guard": {"contains": "THES:ASSERT:write:pass"}},
                    {"any": [
                        {"correlation": {
                            "capture": {"service": "api", "pointer": "/request_id", "json": {"fields": {"/event": "write"}}},
                            "equals": {"service": "worker", "pointer": "/request_id", "json": {"fields": {"/event": "replicated"}}}
                        }},
                        {"guard": {"service": "auditor", "contains": "THES:M:audited"}}
                    ]},
                    {"join": {"endpoints": [
                        {"service": "api", "pointers": ["/request_id", "/attempt"], "json": {"fields": {"/event": "write"}}},
                        {"service": "worker", "pointers": ["/request_id", "/attempt"], "json": {"fields": {"/event": "replicated"}}},
                        {"service": "auditor", "pointers": ["/request_id", "/attempt"], "json": {"fields": {"/event": "audit"}}}
                    ], "quantifier": "every", "occurs": {"exactly": 1}}},
                    {"relation": {
                        "left": {"service": "worker", "pointer": "/attempt", "json": {"fields": {"/event": "replicated"}}},
                        "right": {"service": "api", "pointer": "/attempt", "json": {"fields": {"/event": "write"}}},
                        "operator": "greater_than_or_equal", "quantifier": "every", "occurs": {"exactly": 1}
                    }}
                ]
            }))
            .unwrap(),
        );
        assert!(property_matches_in_run(&joined, &run));
        assert!(campaign_property_description(&joined).contains("worker /attempt >= api /attempt"));
        assert!(campaign_property_description(&joined).contains("exactly 1 distinct source keys"));
        fs::write(
            api.join("serial.log"),
            "THES:ASSERT:write:pass\nTHES:M:written\nTHES:CHECKPOINT:write\n{\"event\":\"write\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"write\",\"request_id\":\"r-18\",\"attempt\":1}\n",
        )
        .unwrap();
        assert!(!property_matches_in_run(&joined, &run));
        fs::write(
            worker.join("serial.log"),
            "THES:M:written\n{\"event\":\"replicated\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"replicated\",\"request_id\":\"r-18\",\"attempt\":1}\n",
        )
        .unwrap();
        fs::write(
            auditor.join("serial.log"),
            "{\"event\":\"audit\",\"request_id\":\"r-17\",\"attempt\":1}\n{\"event\":\"audit\",\"request_id\":\"r-18\",\"attempt\":1}\n",
        )
        .unwrap();
        assert!(!property_matches_in_run(&joined, &run));
        joined.excludes_serial_evidence = Some(
            serde_json::from_value(serde_json::json!({
                "guard": {"service": "auditor", "json": {"fields": {"/event": "audit"}}}
            }))
            .unwrap(),
        );
        assert!(!property_matches_in_run(&joined, &run));
        fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn json_serial_predicates_match_one_complete_event() {
        let predicate = JsonPredicate {
            fields: BTreeMap::from([
                (
                    "/event".to_owned(),
                    serde_json::Value::String("assertion".to_owned()),
                ),
                ("/passed".to_owned(), serde_json::Value::Bool(false)),
            ]),
            where_: Vec::new(),
            arrays: Vec::new(),
            capture: BTreeMap::new(),
            equals_capture: BTreeMap::new(),
        };

        assert!(!serial_matches_json_predicate(
            br#"{"event":"assertion"}
{"passed":false}
"#,
            &predicate,
        ));
        assert!(serial_matches_json_predicate(
            br#"THES:M:read
{"event":"assertion","passed":false}
"#,
            &predicate,
        ));
    }

    #[test]
    fn json_serial_predicates_match_nested_array_records() {
        let predicate: JsonPredicate = serde_json::from_value(serde_json::json!({
            "fields": {"/event": "ready"},
            "arrays": [
                {"pointer": "/checks", "all": {"where": [{"pointer": "/passed", "equals": true}]}},
                {"pointer": "/checks", "any": {"fields": {"/name": "serial"}}},
                {"pointer": "/checks", "none": {"fields": {"/name": "network"}}}
            ]
        }))
        .unwrap();

        assert!(serial_matches_json_predicate(
            br#"{"event":"ready","checks":[{"name":"serial","passed":true},{"name":"disk","passed":true}]}
"#,
            &predicate,
        ));
        assert!(!serial_matches_json_predicate(
            br#"{"event":"ready","checks":[{"name":"serial","passed":false},{"name":"network","passed":true}]}
"#,
            &predicate,
        ));
    }

    #[test]
    fn ordered_serial_predicates_require_each_event_in_order() {
        let predicate: SerialPredicate = serde_json::from_value(serde_json::json!({
            "sequence": [
                {"contains": "THES:CHECKPOINT:write"},
                {"json": {"fields": {"/event": "assertion", "/passed": false}}},
                {"matches": "THES:M:stale"}
            ]
        }))
        .unwrap();

        assert!(serial_matches_nested_predicate(
            b"THES:CHECKPOINT:write\n{\"event\":\"assertion\",\"passed\":false}\nTHES:M:stale\n",
            &predicate,
        ));
        assert!(!serial_matches_nested_predicate(
            b"THES:M:stale\n{\"event\":\"assertion\",\"passed\":false}\nTHES:CHECKPOINT:write\n",
            &predicate,
        ));
    }

    #[test]
    fn ordered_json_serial_predicates_correlate_captured_values() {
        let predicate: SerialPredicate = serde_json::from_value(serde_json::json!({
            "sequence": [
                {"json": {
                    "fields": {"/event": "started"},
                    "capture": {"request": "/request_id"}
                }},
                {"json": {
                    "fields": {"/event": "completed"},
                    "equals_capture": {"/request_id": "request"}
                }}
            ]
        }))
        .unwrap();

        assert!(serial_matches_nested_predicate(
            b"{\"event\":\"started\",\"request_id\":\"r-17\"}\n{\"event\":\"completed\",\"request_id\":\"r-17\"}\n",
            &predicate,
        ));
        assert!(!serial_matches_nested_predicate(
            b"{\"event\":\"started\",\"request_id\":\"r-17\"}\n{\"event\":\"completed\",\"request_id\":\"r-18\"}\n",
            &predicate,
        ));
        assert!(serial_matches_nested_predicate(
            b"{\"event\":\"started\",\"request_id\":\"r-17\"}\n{\"event\":\"completed\",\"request_id\":\"r-18\"}\n{\"event\":\"started\",\"request_id\":\"r-18\"}\n{\"event\":\"completed\",\"request_id\":\"r-18\"}\n",
            &predicate,
        ));
    }

    #[test]
    fn counted_serial_predicates_enforce_the_declared_bounds() {
        let exactly_two: SerialPredicate = serde_json::from_value(serde_json::json!({
            "occurs": {
                "exactly": 2,
                "predicate": {"json": {"fields": {"/event": "retry"}}}
            }
        }))
        .unwrap();
        let bounded: SerialPredicate = serde_json::from_value(serde_json::json!({
            "occurs": {
                "at_least": 2,
                "at_most": 3,
                "predicate": {"matches": "THES:M:retry"}
            }
        }))
        .unwrap();

        let serial = b"{\"event\":\"retry\"}\nTHES:M:retry\n{\"event\":\"retry\"}\nTHES:M:retry\n";
        assert!(serial_matches_nested_predicate(serial, &exactly_two));
        assert!(serial_matches_nested_predicate(serial, &bounded));
        assert!(!serial_matches_nested_predicate(
            b"{\"event\":\"retry\"}\nTHES:M:retry\n",
            &exactly_two,
        ));
        assert!(!serial_matches_nested_predicate(
            b"THES:M:retry\nTHES:M:retry\nTHES:M:retry\nTHES:M:retry\n",
            &bounded,
        ));
    }

    #[test]
    fn json_serial_predicate_conditions_match_one_complete_event() {
        let predicate = JsonPredicate {
            fields: BTreeMap::from([(
                "/event".to_owned(),
                serde_json::Value::String("assertion".to_owned()),
            )]),
            where_: vec![
                JsonCondition {
                    pointer: "/attempt".to_owned(),
                    equals: None,
                    matches: None,
                    greater_than: None,
                    greater_than_or_equal: Some(2.0),
                    less_than: None,
                    less_than_or_equal: None,
                    exists: None,
                },
                JsonCondition {
                    pointer: "/reason".to_owned(),
                    equals: None,
                    matches: Some("^(timeout|reset)$".to_owned()),
                    greater_than: None,
                    greater_than_or_equal: None,
                    less_than: None,
                    less_than_or_equal: None,
                    exists: None,
                },
                JsonCondition {
                    pointer: "/passed".to_owned(),
                    equals: Some(serde_json::Value::Bool(false)),
                    matches: None,
                    greater_than: None,
                    greater_than_or_equal: None,
                    less_than: None,
                    less_than_or_equal: None,
                    exists: None,
                },
                JsonCondition {
                    pointer: "/retryable".to_owned(),
                    equals: None,
                    matches: None,
                    greater_than: None,
                    greater_than_or_equal: None,
                    less_than: None,
                    less_than_or_equal: None,
                    exists: Some(false),
                },
            ],
            arrays: Vec::new(),
            capture: BTreeMap::new(),
            equals_capture: BTreeMap::new(),
        };

        assert!(!serial_matches_json_predicate(
            br#"{"event":"assertion","attempt":2,"reason":"other","passed":false}
{"event":"assertion","attempt":1,"reason":"timeout","passed":false}
"#,
            &predicate,
        ));
        assert!(serial_matches_json_predicate(
            br#"{"event":"assertion","attempt":2,"reason":"timeout","passed":false}
"#,
            &predicate,
        ));
    }

    #[test]
    fn campaign_marker_guards_use_the_restored_parent_transcript() {
        let campaign = CampaignPlan {
            driver: "api".to_owned(),
            operations: vec![
                CampaignOperation {
                    name: "write".to_owned(),
                    input_hex: "77726974650a".to_owned(),
                    stage: None,
                    requires: Vec::new(),
                    excludes: Vec::new(),
                    requires_markers: Vec::new(),
                    excludes_markers: Vec::new(),
                    requires_serial: None,
                    excludes_serial: None,
                    requires_serial_all: Vec::new(),
                    excludes_serial_any: Vec::new(),
                    requires_serial_joins: Vec::new(),
                    excludes_serial_joins: Vec::new(),
                    requires_serial_evidence: None,
                    excludes_serial_evidence: None,
                    max_uses: Some(1),
                },
                CampaignOperation {
                    name: "read".to_owned(),
                    input_hex: "726561640a".to_owned(),
                    stage: None,
                    requires: vec!["write".to_owned()],
                    excludes: Vec::new(),
                    requires_markers: vec!["written".to_owned()],
                    excludes_markers: vec!["closed".to_owned()],
                    requires_serial: None,
                    excludes_serial: None,
                    requires_serial_all: Vec::new(),
                    excludes_serial_any: Vec::new(),
                    requires_serial_joins: Vec::new(),
                    excludes_serial_joins: Vec::new(),
                    requires_serial_evidence: None,
                    excludes_serial_evidence: None,
                    max_uses: Some(1),
                },
            ],
            stages: Vec::new(),
            faults: Vec::new(),
            properties: Vec::new(),
            max_runs: 8,
            max_faults_per_run: 1,
            max_operations_per_run: 2,
        };
        let checkpoint = CampaignCheckpoint {
            switches: BTreeMap::new(),
            services: BTreeMap::new(),
            scheduler: BTreeMap::from([(
                "api".to_owned(),
                ServiceSchedulerCheckpoint {
                    serial_contents: vec![b"THES:M:42\nTHES:M:written\n".to_vec()],
                    program_counters: vec![0x8000],
                    next_fault: 0,
                    paused_until: None,
                    faults: Vec::new(),
                    network_traffic: BTreeMap::new(),
                    network_trace: BTreeMap::new(),
                },
            )]),
            round: 0,
        };

        assert!(campaign_operation_marker_guards_are_ready(
            &campaign,
            &checkpoint,
            1
        ));

        let mut closed = checkpoint.clone();
        closed.scheduler.get_mut("api").unwrap().serial_contents[0]
            .extend_from_slice(b"THES:M:closed\n");
        assert!(!campaign_operation_marker_guards_are_ready(
            &campaign, &closed, 1
        ));

        let mut structured = campaign;
        structured.operations[1].requires_markers.clear();
        structured.operations[1].requires_serial = Some(OperationSerialGuard {
            service: Some("auditor".to_owned()),
            predicate: SerialPredicate {
                contains: None,
                matches: None,
                json: Some(JsonPredicate {
                    fields: BTreeMap::from([
                        (
                            "/event".to_owned(),
                            serde_json::Value::String("assertion".to_owned()),
                        ),
                        ("/passed".to_owned(), serde_json::Value::Bool(false)),
                    ]),
                    where_: vec![JsonCondition {
                        pointer: "/reason".to_owned(),
                        equals: Some(serde_json::Value::String("stale".to_owned())),
                        matches: None,
                        greater_than: None,
                        greater_than_or_equal: None,
                        less_than: None,
                        less_than_or_equal: None,
                        exists: None,
                    }],
                    arrays: Vec::new(),
                    capture: BTreeMap::new(),
                    equals_capture: BTreeMap::new(),
                }),
                all: Vec::new(),
                any: Vec::new(),
                none: Vec::new(),
                sequence: Vec::new(),
                occurs: None,
            },
        });
        structured.operations[1].requires_serial_all = vec![
            OperationSerialGuard {
                service: None,
                predicate: SerialPredicate {
                    contains: Some("THES:M:written".to_owned()),
                    matches: None,
                    json: None,
                    all: Vec::new(),
                    any: Vec::new(),
                    none: Vec::new(),
                    sequence: Vec::new(),
                    occurs: None,
                },
            },
            structured.operations[1].requires_serial.clone().unwrap(),
        ];
        structured.operations[1].excludes_serial_any = vec![OperationSerialGuard {
            service: Some("auditor".to_owned()),
            predicate: SerialPredicate {
                contains: Some("THES:ASSERT:recovered".to_owned()),
                matches: None,
                json: None,
                all: Vec::new(),
                any: Vec::new(),
                none: Vec::new(),
                sequence: Vec::new(),
                occurs: None,
            },
        }];
        structured.operations[1].requires_serial_joins = vec![
            serde_json::from_value(serde_json::json!({
                "endpoints": [
                    {"pointer": "/request_id", "json": {"fields": {"/event": "write"}}},
                    {"service": "auditor", "pointer": "/request_id", "json": {"fields": {"/event": "audit"}}}
                ]
            }))
            .unwrap(),
        ];

        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &checkpoint,
            1
        ));
        let mut stale = checkpoint.clone();
        stale.scheduler.get_mut("api").unwrap().serial_contents[0]
            .extend_from_slice(b"{\"event\":\"write\",\"request_id\":\"r-17\",\"attempt\":1}\n");
        stale.scheduler.insert(
            "auditor".to_owned(),
            ServiceSchedulerCheckpoint {
                serial_contents: vec![
                    b"{\"event\":\"assertion\",\"passed\":false,\"reason\":\"stale\"}\n{\"event\":\"audit\",\"request_id\":\"r-17\",\"attempt\":2}\n".to_vec(),
                ],
                program_counters: Vec::new(),
                next_fault: 0,
                paused_until: None,
                faults: Vec::new(),
                network_traffic: BTreeMap::new(),
                network_trace: BTreeMap::new(),
            },
        );
        assert!(campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            1
        ));
        structured.operations[1].excludes_serial_joins =
            structured.operations[1].requires_serial_joins.clone();
        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            1
        ));
        structured.operations[1].excludes_serial_joins.clear();
        structured.operations[1].requires_serial = None;
        structured.operations[1].requires_serial_all.clear();
        structured.operations[1].excludes_serial_any.clear();
        structured.operations[1].requires_serial_joins.clear();
        structured.operations[1].requires_serial_evidence = Some(
            serde_json::from_value(serde_json::json!({
                "all": [
                    {"guard": {"contains": "THES:M:written"}},
                    {"join": {"endpoints": [
                        {"pointer": "/request_id", "json": {"fields": {"/event": "write"}}},
                        {"service": "auditor", "pointer": "/request_id", "json": {"fields": {"/event": "audit"}}}
                    ], "quantifier": "every", "occurs": {"exactly": 1}}},
                    {"any": [
                        {"correlation": {
                            "capture": {"pointer": "/request_id", "json": {"fields": {"/event": "write"}}},
                            "equals": {"service": "auditor", "pointer": "/request_id", "json": {"fields": {"/event": "audit"}}}
                        }},
                        {"guard": {"service": "auditor", "contains": "THES:M:audited"}}
                    ]},
                    {"relation": {
                        "left": {"service": "auditor", "pointer": "/attempt", "json": {"fields": {"/event": "audit"}}},
                        "right": {"pointer": "/attempt", "json": {"fields": {"/event": "write"}}},
                        "operator": "greater_than", "quantifier": "every", "occurs": {"exactly": 1}
                    }}
                ]
            }))
            .unwrap(),
        );
        structured.operations[1].excludes_serial_evidence = Some(
            serde_json::from_value(serde_json::json!({
                "guard": {"service": "auditor", "contains": "THES:ASSERT:recovered"}
            }))
            .unwrap(),
        );
        assert!(campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            1
        ));
        stale.scheduler.get_mut("api").unwrap().serial_contents[0]
            .extend_from_slice(b"{\"event\":\"write\",\"request_id\":\"r-18\",\"attempt\":1}\n");
        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            1
        ));
        stale.scheduler.get_mut("auditor").unwrap().serial_contents[0]
            .extend_from_slice(b"{\"event\":\"audit\",\"request_id\":\"r-18\",\"attempt\":2}\n");
        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            1
        ));
        stale.scheduler.get_mut("auditor").unwrap().serial_contents[0]
            .extend_from_slice(b"THES:ASSERT:recovered\n");
        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            1
        ));
    }

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
    fn campaign_selection_seeds_each_root_operation_before_extensions() {
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
                novel_state: false,
                failed: false,
            }],
        );

        assert_eq!(selected, 0);
        assert_eq!(reason, "canonical breadth-first operation seed");
    }

    #[test]
    fn campaign_selection_extends_a_new_topology_state_prefix() {
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
            &[2],
            &[CampaignGuidanceObservation {
                operations: vec![0],
                novel_markers: 0,
                novel_state: true,
                failed: false,
            }],
        );

        assert_eq!(selected, 0);
        assert_eq!(reason, "extends 1-operation prefix with new topology state");
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
        assert_eq!(reason, "canonical breadth-first operation seed");
    }

    #[test]
    fn campaign_replay_verifies_guidance_evidence() {
        let actual = CampaignRun {
            index: 0,
            operations: vec!["write".to_owned()],
            fault: None,
            faults: vec!["backplane:partition@write".to_owned()],
            actions: Vec::new(),
            selection: "extends 1-operation prefix with new topology state".to_owned(),
            program_counters: BTreeMap::from([("api".to_owned(), vec!["0x8000".to_owned()])]),
            state_sha256: "state".to_owned(),
            state_novel: true,
            status: "passed",
            novelty: vec!["checkpoint".to_owned()],
        };
        let expected = RecordedCampaignRun {
            operations: actual.operations.clone(),
            fault: None,
            faults: actual.faults.clone(),
            actions: Vec::new(),
            selection: actual.selection.clone(),
            program_counters: actual.program_counters.clone(),
            novelty: actual.novelty.clone(),
            state_sha256: actual.state_sha256.clone(),
            state_novel: true,
            status: actual.status.to_owned(),
        };

        assert!(campaign_replay_mismatches(&expected, &actual).is_empty());
        let mut changed = actual;
        changed.state_sha256 = "other-state".to_owned();
        assert_eq!(
            campaign_replay_mismatches(&expected, &changed),
            vec!["topology-state coverage"]
        );
    }

    #[test]
    fn campaign_topology_state_ignores_serial_output() {
        let directory =
            std::env::temp_dir().join(format!("theseus-topology-state-{}", std::process::id()));
        let service = directory.join("services/api");
        fs::create_dir_all(&service).unwrap();
        fs::write(
            service.join("result.json"),
            r#"{"serial_sha256":["first"],"storage_sha256":{"data":"drive"},"network_traffic":{"backplane":{"tx_frames":1,"rx_frames":2,"dropped":0}},"virtual_time_ns":[100]}"#,
        )
        .unwrap();
        let first = campaign_topology_state_sha256(&directory, &BTreeMap::new()).unwrap();
        fs::write(
            service.join("result.json"),
            r#"{"serial_sha256":["second"],"storage_sha256":{"data":"drive"},"network_traffic":{"backplane":{"tx_frames":1,"rx_frames":2,"dropped":0}},"virtual_time_ns":[100]}"#,
        )
        .unwrap();

        assert_eq!(
            first,
            campaign_topology_state_sha256(&directory, &BTreeMap::new()).unwrap()
        );
        fs::remove_dir_all(directory).unwrap();
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
    fn campaign_operation_histories_cover_order_and_repetition_breadth_first() {
        assert_eq!(
            ordered_operation_histories(2, 3, |_, _| true),
            vec![
                vec![0],
                vec![1],
                vec![0, 0],
                vec![0, 1],
                vec![1, 0],
                vec![1, 1],
                vec![0, 0, 0],
                vec![0, 0, 1],
                vec![0, 1, 0],
                vec![0, 1, 1],
                vec![1, 0, 0],
                vec![1, 0, 1],
                vec![1, 1, 0],
                vec![1, 1, 1],
            ]
        );
    }

    #[test]
    fn campaign_operation_histories_obey_preconditions() {
        assert_eq!(
            ordered_operation_histories(2, 3, |history, operation| {
                operation == 0 || history.contains(&0)
            }),
            vec![
                vec![0],
                vec![0, 0],
                vec![0, 1],
                vec![0, 0, 0],
                vec![0, 0, 1],
                vec![0, 1, 0],
                vec![0, 1, 1],
            ]
        );
    }

    #[test]
    fn campaign_operation_histories_obey_exclusions_and_use_bounds() {
        assert_eq!(
            ordered_operation_histories(2, 3, |history, operation| {
                let writes = history.iter().filter(|prior| **prior == 0).count();
                match operation {
                    0 => writes < 1,
                    1 => history.contains(&0) && !history.contains(&1),
                    _ => false,
                }
            }),
            vec![vec![0], vec![0, 1]]
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
    fn campaign_delta_debugging_removes_large_irrelevant_chunks() {
        let mut attempted_lengths = Vec::new();
        let (reduced, attempts) = minimize_campaign_items((0..8).collect(), 1, |candidate| {
            attempted_lengths.push(candidate.len());
            Ok(candidate.contains(&7))
        })
        .unwrap();

        assert_eq!(reduced, vec![7]);
        assert!(attempted_lengths.contains(&4));
        assert_eq!(attempts, attempted_lengths.len());
    }

    #[test]
    fn campaign_delta_debugging_can_remove_every_fault() {
        let (reduced, attempts) = minimize_campaign_items(vec![2, 7, 11], 0, |_| Ok(true)).unwrap();

        assert!(reduced.is_empty());
        assert!(attempts > 0);
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
