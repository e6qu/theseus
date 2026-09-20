// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Linux+KVM executor for a normalized Theseus Compose topology plan.
//!
//! This stays separate from the portable `theseus` CLI: the CLI plans on
//! macOS, while this binary links Firecracker's Linux/KVM VMM and runs only
//! from a published Linux runtime bundle.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::net::Ipv4Addr;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use addr2line::Loader;
use object::{BinaryFormat, Object, ObjectKind, ObjectSection, ObjectSymbol, SymbolKind};
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use serde_json_path::JsonPath;
use sha2::{Digest, Sha256};
use theseus_engine::simnet::{SharedSimSwitch, SimSwitch, SimSwitchState};
use theseus_orchestrator::branch::BranchPoint;
use vmm::builder::build_microvm_for_boot;
use vmm::devices::virtio::block::device::Block;
use vmm::devices::virtio::block::virtio::device::SimulatedBlockConfig;
use vmm::devices::virtio::net::{
    Net, SimNetConfig, SimNetDropReason, SimNetFrameDirection, SimNetPacketSelector, SimNetState,
};
use vmm::persist::{restore_from_microvm_state, VmInfo};
use vmm::rate_limiter::RateLimiter;
use vmm::resources::VmResources;
use vmm::seccomp::get_empty_filters;
use vmm::utils::net::mac::MacAddr;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::entropy::EntropyDeviceConfig;
use vmm::vmm_config::instance_info::InstanceInfo;
use vmm::vmm_config::machine_config::{MachineConfigUpdate, VirtualTimeConfig};
use vmm::vmm_config::snapshot::{
    LoadSnapshotParams, MemBackendConfig, MemBackendType, SnapshotLoadHugePageConfig,
};
use vmm::{
    EventManager, ExecutionLedger, ExecutionLedgerEvidence, FcExitCode, MachineExecutionState, Vmm,
};

mod checkpoint_cache;
mod starting_state;

const USAGE: &str = "Usage:
  theseus-topology --plan topology-plan.json --output replay-dir [--minimize]
  theseus-topology certify --plan topology-plan.json --output certificate-dir";
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
    #[serde(default)]
    replay_start: starting_state::ReplayStart,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    starting_checkpoint: Option<Artifact>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    checkpoint_prefixes: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "MachineReplayMode::is_exact")]
    machine_replay: MachineReplayMode,
    /// Global service order for plan-level events. Campaign exports need this
    /// because per-service event arrays cannot represent cross-service order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    event_order: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MachineReplayMode {
    #[default]
    Exact,
    HostInputs,
}

impl MachineReplayMode {
    fn is_exact(&self) -> bool {
        *self == Self::Exact
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct CampaignPlan {
    driver: String,
    #[serde(default)]
    fault_profile: Option<CampaignFaultProfile>,
    #[serde(default)]
    test_template: Option<String>,
    #[serde(default)]
    test_templates: Vec<String>,
    #[serde(default = "default_test_command_parallelism")]
    max_parallel_commands: u8,
    /// The deterministic policy used to order an otherwise fixed campaign
    /// corpus. The locked replay plan retains this choice for inspection;
    /// replay itself executes the recorded schedule order.
    #[serde(default)]
    guidance: CampaignGuidance,
    /// The deterministic coverage evidence used by the corpus scheduler.
    #[serde(default)]
    coverage: CampaignCoverage,
    #[serde(default)]
    state: BTreeMap<String, String>,
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

fn default_test_command_parallelism() -> u8 {
    2
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_campaign_operations_per_run() -> u8 {
    3
}

/// Campaign selection remains deterministic for a fixed plan and seed. Legacy
/// plans default to coverage; newly normalized plans explicitly select the
/// unified policy. All policies use retained observations, not a remote or
/// nondeterministic model.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignGuidance {
    #[default]
    Coverage,
    Adaptive,
    Posterior,
    Property,
    /// Rank the complete controlled decision prefix using coverage, state,
    /// property, structured-choice, scheduling, and fault evidence together.
    Unified,
}

/// Select one primary coverage signal when comparing scheduler strategies.
/// Topology-state and failure evidence remains a shared secondary signal.
/// `execution_locations` is the practical default; the other modes remain
/// reproducible baselines for evaluating its value on a campaign workload.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignCoverage {
    Markers,
    CheckpointPcs,
    #[default]
    ExecutionLocations,
    ApplicationBlocks,
    ApplicationEdges,
}

#[derive(Debug, Deserialize, Serialize)]
struct CampaignOperation {
    name: String,
    #[serde(default)]
    test_template: Option<String>,
    #[serde(default)]
    service: String,
    #[serde(default)]
    command: Option<CampaignTestCommand>,
    #[serde(default)]
    test_command_path: Option<String>,
    #[serde(default)]
    shell_phase: Option<CampaignShellPhase>,
    #[serde(default)]
    shell_process: Option<String>,
    #[serde(default)]
    thread_schedule: Vec<u8>,
    #[serde(default)]
    thread_schedule_search: Option<CampaignThreadScheduleSearch>,
    #[serde(default)]
    thread_schedule_exploration: Option<CampaignThreadScheduleExploration>,
    /// Bounds for named choices made by the running command. Concrete input
    /// cases retain the selected values used for this execution.
    #[serde(default)]
    choice_bounds: BTreeMap<String, u16>,
    /// `input_hex` is retained only to replay plans locked by older Theseus
    /// releases. New plans always use named `inputs`.
    #[serde(default)]
    input_hex: Option<String>,
    #[serde(default)]
    inputs: Vec<CampaignOperationInput>,
    /// Retained for offline inspection. Execution uses the already-expanded
    /// input cases, so a replay cannot change with grammar implementation.
    #[serde(default)]
    input_grammar: Option<CampaignOperationInputGrammar>,
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
    #[serde(default)]
    requires_state: BTreeMap<String, String>,
    #[serde(default)]
    sets_state: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignTestCommand {
    First,
    ParallelDriver,
    SerialDriver,
    SingletonDriver,
    Anytime,
    Eventually,
    Finally,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignShellPhase {
    Run,
    Setup,
    Launch,
    Completion,
    Assertion,
    Recovery,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CampaignOperationInputGrammar {
    template: String,
    name_template: String,
    choices: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    input_captures: BTreeMap<String, CampaignOperationInputCapture>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CampaignOperationInput {
    name: String,
    input_hex: String,
    #[serde(default)]
    choices: BTreeMap<String, u16>,
    #[serde(default)]
    thread_schedule: Vec<u8>,
    #[serde(default)]
    input_template: Option<String>,
    #[serde(default)]
    input_captures: BTreeMap<String, CampaignOperationInputCapture>,
    #[serde(default)]
    requires: Vec<CampaignOperationInputReference>,
    #[serde(default)]
    excludes: Vec<CampaignOperationInputReference>,
    #[serde(default)]
    max_uses: Option<u8>,
    #[serde(default)]
    requires_state: BTreeMap<String, String>,
    #[serde(default)]
    sets_state: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CampaignThreadScheduleSearch {
    threads: Vec<u8>,
    period: u8,
    max_switches: u8,
    generated_schedules: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CampaignThreadScheduleExploration {
    strategy: String,
    max_choices: u8,
    max_variants: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CampaignOperationInputCapture {
    #[serde(default)]
    service: Option<String>,
    pointer: String,
    #[serde(default)]
    json: Option<JsonPredicate>,
    #[serde(default)]
    sequence: Vec<SerialPredicate>,
    #[serde(default)]
    workflow: Option<SerialWorkflow>,
    #[serde(default)]
    encoding: CampaignOperationInputEncoding,
    #[serde(default)]
    select: CampaignOperationInputSelect,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignOperationInputEncoding {
    #[default]
    Text,
    Json,
    Hex,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignOperationInputSelect {
    First,
    #[default]
    Latest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CampaignOperationInputReference {
    operation: String,
    #[serde(default)]
    input: Option<String>,
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
    #[serde(default, skip_serializing_if = "is_false")]
    required: bool,
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
    after_input: Option<CampaignOperationInputReference>,
    #[serde(default)]
    at_round: Option<u64>,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<i64>,
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
    #[serde(default)]
    every_n_rounds: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignFaultKind {
    Pause,
    Restart,
    ClockJump,
    CpuThrottle,
    CpuRelease,
    Partition,
    Heal,
    LinkPartition,
    LinkHeal,
    LinkClog,
    LinkUnclog,
    LinkFault,
    LinkRecover,
    ServiceStop,
    ServiceStart,
    ServiceKill,
    ServiceRestart,
    StorageFault,
    StorageRecover,
    NetworkFault,
    NetworkRecover,
    PacketFault,
    PacketRecover,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CampaignFaultProfile {
    Standard,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SerialWorkflow {
    pointers: Vec<String>,
    stages: Vec<SerialWorkflowStage>,
    #[serde(default)]
    quantifier: SerialJoinQuantifier,
    #[serde(default)]
    occurs: Option<SerialMatchCount>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SerialWorkflowStage {
    service: String,
    #[serde(default)]
    pointers: Vec<String>,
    steps: Vec<JsonPredicate>,
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
    #[serde(default)]
    workflow: Option<SerialWorkflow>,
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
    query: Option<String>,
    #[serde(default)]
    fields: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "where")]
    where_: Vec<JsonCondition>,
    #[serde(default)]
    arrays: Vec<JsonArrayPredicate>,
    #[serde(default)]
    all: Vec<JsonPredicate>,
    #[serde(default)]
    any: Vec<JsonPredicate>,
    #[serde(default)]
    none: Vec<JsonPredicate>,
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
    AlwaysOrUnreachable,
    Sometimes,
    Reachable,
    Unreachable,
}

#[derive(Debug, Serialize)]
struct CampaignResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    starting_checkpoint_sha256: Option<String>,
    format: &'static str,
    decision_trace_format: &'static str,
    status: &'static str,
    driver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_template: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    test_templates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_parallel_commands: Option<u8>,
    guidance: CampaignGuidance,
    coverage: CampaignCoverage,
    checkpoint_nodes: usize,
    checkpoint_reuses: usize,
    generated_candidates: usize,
    marker_guard_rejections: usize,
    serial_guard_rejections: usize,
    unique_topology_states: usize,
    unique_instruction_locations: usize,
    unique_application_blocks: usize,
    unique_application_edges: usize,
    thread_scheduling_decisions: usize,
    thread_synchronization_events: usize,
    execution_decisions: u64,
    structured_choice_decisions: usize,
    /// A compact, deterministic account of the search work. This is separate
    /// from wall-clock timing: host scheduling must never affect a replay.
    search: CampaignSearchEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay_verification: Option<CampaignReplayVerification>,
    runs: Vec<CampaignRun>,
    properties: Vec<CampaignPropertyResult>,
}

#[derive(Debug, Serialize)]
struct CampaignRun {
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_template: Option<String>,
    operations: Vec<String>,
    /// Canonical, human-readable execution decisions in boundary order. This
    /// is replay-checked in addition to the richer typed evidence below.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    decision_trace: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    thread_schedule_prefixes: Vec<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fault: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    faults: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    actions: Vec<AppliedCampaignAction>,
    selection: String,
    /// The complete prior-observation ledger which the scheduler saw when it
    /// selected this candidate. Its digest makes guidance auditable without
    /// embedding an O(n²) copy of the corpus in every run.
    guidance_ledger: CampaignGuidanceLedger,
    #[serde(skip_serializing_if = "Option::is_none")]
    guidance_evidence: Option<CampaignPosteriorEvidence>,
    #[serde(default)]
    property_witnesses: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    timeline: Vec<CampaignTimelineBoundary>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    program_counters: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    instruction_locations: BTreeMap<String, Vec<InstructionLocation>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    instruction_novelty: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    application_blocks: BTreeMap<String, Vec<ApplicationBlock>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    application_block_novelty: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    thread_scheduling: BTreeMap<String, Vec<ThreadSchedulingDecision>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    thread_synchronization: BTreeMap<String, Vec<ThreadSynchronizationEvent>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    structured_choices: BTreeMap<String, Vec<StructuredChoiceDecision>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    execution_ledgers: BTreeMap<String, Vec<ExecutionLedgerEvidence>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    machine_execution_ledgers: BTreeMap<String, ExecutionLedgerEvidence>,
    /// Complete VM-wide decisions. Replay uses these as an admission protocol,
    /// rather than merely comparing a digest after execution has finished.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    machine_execution_traces: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    checkpoint_pc_novelty: Vec<String>,
    state_sha256: String,
    state_novel: bool,
    status: &'static str,
    novelty: Vec<String>,
}

/// One deterministic operation boundary. This deliberately records compact
/// checkpoint evidence rather than a hardware instruction trace: the full
/// serial log and VM snapshot remain in the locked run directory.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignTimelineBoundary {
    /// Stable within one recorded schedule and reproduced verbatim on replay.
    #[serde(default)]
    id: String,
    operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<CampaignTestCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    test_command_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    terminated_command_services: Vec<String>,
    /// The UART service that received this operation. Empty only in a result
    /// recorded before service-targeted operations existed.
    #[serde(default)]
    service: String,
    /// A bounded, escaped view of the exact bytes delivered to this service.
    /// The replay plan retains the complete event corpus.
    #[serde(default)]
    input: CampaignInputEvidence,
    /// The UART receipt establishes what happened to the operation input: the
    /// emulator accepted every byte, and the paused checkpoints record how
    /// much the guest consumed and left queued.
    #[serde(default)]
    delivery: CampaignUartDelivery,
    /// A marker barrier is valid only when it appears after this operation's
    /// UART bytes were accepted. This records that post-input response window.
    #[serde(default)]
    barrier: CampaignUartBarrier,
    round: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    actions: Vec<AppliedCampaignAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    markers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    new_markers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    changed_program_counters: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    changed_serial: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    program_counters: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    instruction_locations: BTreeMap<String, Vec<InstructionLocation>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    application_blocks: BTreeMap<String, Vec<ApplicationBlock>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    new_application_blocks: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    thread_scheduling: BTreeMap<String, Vec<ThreadSchedulingDecision>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    new_thread_scheduling_decisions: BTreeMap<String, Vec<ThreadSchedulingDecision>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    thread_synchronization: BTreeMap<String, Vec<ThreadSynchronizationEvent>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    new_thread_synchronization_events: BTreeMap<String, Vec<ThreadSynchronizationEvent>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    structured_choices: BTreeMap<String, Vec<StructuredChoiceDecision>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    new_structured_choices: BTreeMap<String, Vec<StructuredChoiceDecision>>,
    serial_sha256: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    serial_delta: BTreeMap<String, CampaignSerialDelta>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    network_traffic_delta: BTreeMap<String, BTreeMap<String, CampaignNetworkTrafficDelta>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    changed_storage: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    virtual_time_delta_ns: BTreeMap<String, Vec<u64>>,
    /// Complete rolling identity of ordered guest-visible KVM exits at this
    /// boundary. Replay compares it exactly and therefore rejects execution
    /// that reaches the same output through a different low-level path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    execution_ledgers: BTreeMap<String, Vec<ExecutionLedgerEvidence>>,
    /// One total order of handled exits, device effects, and explicit host
    /// inputs across all vCPUs for each service VM.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    machine_execution_ledgers: BTreeMap<String, ExecutionLedgerEvidence>,
    state_sha256: String,
}

/// A bounded, escaped excerpt of one service's serial bytes emitted between
/// adjacent operation checkpoints. The hash always covers the complete delta,
/// including bytes omitted from the excerpt.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignSerialDelta {
    bytes: usize,
    sha256: String,
    excerpt: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    omitted_bytes: usize,
}

/// The complete UART input remains in the locked replay plan. This compact
/// copy makes a boundary understandable in a portable result without allowing
/// one unusually large input to dominate the report.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignInputEvidence {
    bytes: usize,
    sha256: String,
    excerpt: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    omitted_bytes: usize,
}

/// Exact UART queue accounting for one operation. `accepted_bytes` is all or
/// nothing: a full FIFO rejects the operation before it can be checkpointed.
/// `guest_read_bytes` can include older queued input, so the before/after
/// depths remain explicit rather than implying every read belongs to this
/// operation.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignUartDelivery {
    recorded: bool,
    accepted_bytes: usize,
    pending_before: usize,
    pending_after: usize,
    guest_read_bytes: usize,
    /// The serial marker awaited before applying this operation's actions.
    /// Empty means the operation deliberately had no marker barrier.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    checkpoint: String,
}

/// The target service's post-input serial evidence through the first matched
/// operation barrier. The marker offset is relative to this response, never a
/// reused historical log position.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignUartBarrier {
    recorded: bool,
    checkpoint: String,
    marker_offset: usize,
    /// The deterministic topology round in which the response was observed.
    round: u64,
    response: CampaignSerialDelta,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn campaign_input_evidence(data_hex: &str) -> CampaignInputEvidence {
    let input = decode_hex(data_hex).expect("campaign input is normalized hexadecimal");
    let excerpt = &input[..input.len().min(CAMPAIGN_EVIDENCE_EXCERPT_BYTES)];
    let mut hasher = Sha256::new();
    hasher.update(&input);
    CampaignInputEvidence {
        bytes: input.len(),
        sha256: format!("{:x}", hasher.finalize()),
        excerpt: excerpt
            .iter()
            .flat_map(|byte| std::ascii::escape_default(*byte))
            .map(char::from)
            .collect(),
        omitted_bytes: input.len() - excerpt.len(),
    }
}

/// Counter changes observed at an operation boundary. Payload digests remain
/// in the locked per-service result; these counters make a topology effect
/// visible in the compact timeline.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignNetworkTrafficDelta {
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    tx_frames: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    rx_frames: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    dropped: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    duplicated: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    corrupted: u64,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// The deterministic posterior used to choose a campaign action. `successes`
/// counts runs which yielded a marker, paused-PC location, topology state, or
/// failure; `misses` counts runs without any of those outcomes. The uniform
/// Beta(1, 1) prior makes an untried action explicit rather than silently
/// treating it as either good or bad.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignPosteriorEvidence {
    action: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    context: Vec<String>,
    scope: String,
    successes: usize,
    misses: usize,
    mean_per_mille: usize,
    uncertainty_per_mille: usize,
    score: usize,
}

/// Deterministic search work performed by one campaign. The values are
/// logical topology restores and captures, rather than elapsed time, so they
/// stay meaningful and replayable on differently loaded hosts.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignCheckpointEconomics {
    root_captures: usize,
    prefix_captures: usize,
    checkpoint_nodes: usize,
    prefix_reuses: usize,
    prefix_restores: usize,
    leaf_restores: usize,
    topology_restores: usize,
    avoided_prefix_recomputations: usize,
    /// Immutable memfds retained for this campaign. These bytes replace
    /// per-checkpoint `memory.snap` files and are released with the tree.
    retained_memory_bytes: u64,
    /// Logical bytes mapped from immutable memfds by prefix and leaf restores.
    /// Linux MAP_PRIVATE shares clean pages and COWs writes per child.
    shared_cow_restore_bytes: u64,
    /// KVM dirty-page footprint sampled at capture barriers. This is a stable
    /// logical write-set measure, not host RSS accounting.
    private_dirty_pages: u64,
    /// In-memory prefix snapshot bytes written to files (currently zero).
    /// Excludes durable starting-root exports and locked runtime/guest inputs.
    snapshot_file_bytes: u64,
    /// Deterministic LRU removals from the bounded prefix RAM cache.
    #[serde(default)]
    prefix_evictions: usize,
}

/// Global record that the checkpoint tree and the inputs to guidance were the
/// same during replay. Per-run evidence explains individual choices; this
/// record catches changes to the search as a whole.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignSearchEvidence {
    checkpoint: CampaignCheckpointEconomics,
    guidance_observations: usize,
    guidance_sha256: String,
}

/// The scheduler's deterministic input at one choice point. Operation names
/// and outcomes are hashed in stable declaration order, keeping result files
/// compact while making every later scheduling decision replay-verifiable.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
struct CampaignGuidanceLedger {
    observations: usize,
    sha256: String,
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
    starting_checkpoint_sha256: Option<String>,
    #[serde(default)]
    guidance: Option<CampaignGuidance>,
    #[serde(default)]
    coverage: Option<CampaignCoverage>,
    #[serde(default)]
    generated_candidates: usize,
    #[serde(default)]
    search: Option<CampaignSearchEvidence>,
    runs: Vec<RecordedCampaignRun>,
}

#[derive(Debug, Clone, Deserialize)]
struct RecordedCampaignRun {
    #[serde(default)]
    test_template: Option<String>,
    operations: Vec<String>,
    #[serde(default)]
    decision_trace: Vec<String>,
    #[serde(default)]
    thread_schedule_prefixes: Vec<Vec<u8>>,
    #[serde(default)]
    fault: Option<String>,
    #[serde(default)]
    faults: Vec<String>,
    #[serde(default)]
    actions: Vec<AppliedCampaignAction>,
    #[serde(default)]
    selection: String,
    #[serde(default)]
    guidance_ledger: Option<CampaignGuidanceLedger>,
    #[serde(default)]
    guidance_evidence: Option<CampaignPosteriorEvidence>,
    #[serde(default)]
    property_witnesses: Option<Vec<String>>,
    #[serde(default)]
    timeline: Vec<CampaignTimelineBoundary>,
    #[serde(default)]
    program_counters: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    instruction_locations: BTreeMap<String, Vec<InstructionLocation>>,
    #[serde(default)]
    instruction_novelty: Vec<String>,
    #[serde(default)]
    application_blocks: BTreeMap<String, Vec<ApplicationBlock>>,
    #[serde(default)]
    application_block_novelty: Vec<String>,
    #[serde(default)]
    thread_scheduling: BTreeMap<String, Vec<ThreadSchedulingDecision>>,
    #[serde(default)]
    thread_synchronization: BTreeMap<String, Vec<ThreadSynchronizationEvent>>,
    #[serde(default)]
    structured_choices: BTreeMap<String, Vec<StructuredChoiceDecision>>,
    #[serde(default)]
    execution_ledgers: BTreeMap<String, Vec<ExecutionLedgerEvidence>>,
    #[serde(default)]
    machine_execution_ledgers: BTreeMap<String, ExecutionLedgerEvidence>,
    #[serde(default)]
    machine_execution_traces: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    checkpoint_pc_novelty: Vec<String>,
    #[serde(default)]
    novelty: Vec<String>,
    #[serde(default)]
    state_sha256: String,
    #[serde(default)]
    state_novel: bool,
    #[serde(default)]
    status: String,
}

/// A paused-PC sample enriched from the locked service kernel's ELF. The
/// numeric address remains the replay identity; function and source data are
/// best-effort explanations for people and are absent for stripped kernels.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct InstructionLocation {
    address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<InstructionSourceLocation>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct InstructionSourceLocation {
    file: String,
    line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<u32>,
}

/// A compiler-emitted application coverage point. GCC v1 records identify a
/// block by its module-relative address, Go v1 records use a fixed-executable
/// program counter, and LLVM v2 records add a build-local edge number. The
/// build digest scopes every form across rebuilds.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct ApplicationBlock {
    process: String,
    module: String,
    build_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge: Option<u32>,
    offset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symbol_offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<InstructionSourceLocation>,
}

/// One compiler-controlled userspace scheduling choice. Thread identities are
/// assigned in pthread creation order; the build digest and module-relative
/// point prevent an unrelated build or ASLR relocation from matching it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct ThreadSchedulingDecision {
    process: String,
    module: String,
    build_sha256: String,
    decision: u64,
    from_thread: u8,
    runnable_mask: String,
    selected_thread: u8,
    point_offset: String,
}

/// One controlled pthread synchronization transition. Synchronization objects
/// use first-use identities assigned by the instrumented process, never host
/// addresses, so this evidence can be compared across replay and ASLR.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct ThreadSynchronizationEvent {
    process: String,
    module: String,
    build_sha256: String,
    event: u64,
    thread: u8,
    operation: String,
    object_kind: String,
    object: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_thread: Option<u8>,
}

/// One named value consumed by a workload at the decision point. The input
/// case fixes the value; the emitted record proves the workload actually used
/// it with the declared exclusive bound.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct StructuredChoiceDecision {
    ordinal: u64,
    name: String,
    upper_exclusive: u16,
    selected: u16,
}

#[derive(Debug, Serialize)]
struct CampaignMinimization {
    property: String,
    original_operations: Vec<String>,
    minimized_operations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    original_thread_schedule_prefixes: Vec<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    minimized_thread_schedule_prefixes: Vec<Vec<u8>>,
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
    depends_on: Vec<DependencyPlan>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<theseus_orchestrator::oci::ContainerLaunch>,
    #[serde(default)]
    configs: Vec<theseus_orchestrator::oci::ContainerConfig>,
    #[serde(default)]
    secrets: Vec<theseus_orchestrator::oci::ContainerConfig>,
    #[serde(default)]
    volumes: Vec<theseus_orchestrator::oci::ContainerVolume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    healthcheck: Option<theseus_orchestrator::oci::ContainerHealthcheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
    #[serde(default)]
    extra_hosts: BTreeMap<String, String>,
    #[serde(default)]
    faults: Vec<FaultPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    coverage: Vec<CoverageArtifact>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CoverageArtifact {
    #[serde(default = "llvm_coverage_format")]
    format: String,
    #[serde(default = "edge_coverage_kind")]
    coverage: String,
    language: String,
    process: String,
    module: String,
    build_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gnu_build_id: Option<String>,
    manifest: Artifact,
    symbols: Artifact,
}

#[derive(Debug, Deserialize)]
struct CoverageManifest {
    format: String,
    coverage: String,
    language: String,
    process: String,
    module: String,
    build_sha256: String,
    #[serde(default)]
    gnu_build_id: Option<String>,
    symbols: String,
}

fn llvm_coverage_format() -> String {
    "theseus-llvm-coverage-build-v1".to_owned()
}

fn edge_coverage_kind() -> String {
    "edges".to_owned()
}

#[derive(Debug, Deserialize, Serialize)]
struct DependencyPlan {
    service: String,
    condition: DependencyCondition,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DependencyCondition {
    ServiceStarted,
    ServiceHealthy,
}

#[derive(Debug, Deserialize, Serialize)]
struct FaultPlan {
    at_round: u64,
    kind: FaultKind,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<i64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container_service: Option<theseus_orchestrator::oci::ContainerServiceContract>,
    /// Derived while locking a Compose topology. It is intentionally separate
    /// from `container_service`, so a plain image entrypoint can use the
    /// deterministic network without opting into Theseus service checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container_network: Option<theseus_orchestrator::oci::ContainerNetwork>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RuntimePlan {
    firecracker: Artifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_adapter: Option<Artifact>,
}
#[derive(Debug, Deserialize, Serialize)]
struct GuestPlan {
    kernel: Artifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initramfs: Option<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<Artifact>,
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
    #[serde(default = "default_max_rounds")]
    max_rounds: u64,
    virtual_time: Option<VirtualTime>,
}
#[derive(Debug, Deserialize, Serialize)]
struct VirtualTime {
    tick_ns: u64,
    exits_per_tick: u32,
    #[serde(default)]
    hold_kernel_timers: bool,
}

fn default_max_rounds() -> u64 {
    10_000_000
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
    /// An explicit serial barrier supplied by a manifest or campaign
    /// operation. The next input is withheld until this complete line reaches
    /// the host transcript.
    #[serde(default)]
    checkpoint: Option<String>,
    /// Topology mutations deliberately occur only after the event's serial
    /// checkpoint, so the next operation observes the new state.
    #[serde(default)]
    actions: Vec<CampaignAction>,
}

/// A campaign event retains its UART target alongside its bytes. Ordinary
/// topology plans keep their per-service event lists; this wrapper exists only
/// while checkpoints share a mixed-service campaign prefix.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CampaignEvent {
    service: String,
    /// An eventually command kills every service-local test command still
    /// live at this exact decision prefix before its check begins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    terminate_shell_processes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recover_faults: Vec<CampaignAction>,
    event: EventPlan,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_rounds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    every_n_rounds: Option<u32>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_start: Option<starting_state::ExecutionStart>,
    serial_log: String,
    serial_logs: Vec<String>,
    serial_sha256: Vec<String>,
    faults_sha256: String,
    storage_sha256: BTreeMap<String, String>,
    network_traffic: BTreeMap<String, NetworkTraffic>,
    network_trace: BTreeMap<String, Vec<NetworkFrame>>,
    entropy_probe_sha256: String,
    virtual_time_ns: Option<Vec<u64>>,
    execution_ledgers: Vec<ExecutionLedgerEvidence>,
    machine_execution_ledger: ExecutionLedgerEvidence,
    machine_execution_trace: Vec<String>,
    error: Option<String>,
    checks: Vec<CheckResult>,
    faults: Vec<AppliedFault>,
}

#[derive(Debug, Deserialize)]
struct RecordedServiceResult {
    #[serde(default)]
    execution_start: Option<starting_state::ExecutionStart>,
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
    entropy_probe_sha256: Option<String>,
    #[serde(default)]
    virtual_time_ns: Option<Option<Vec<u64>>>,
    #[serde(default)]
    execution_ledgers: Option<Vec<ExecutionLedgerEvidence>>,
    #[serde(default)]
    machine_execution_ledger: Option<ExecutionLedgerEvidence>,
    #[serde(default)]
    machine_execution_trace: Option<Vec<String>>,
}

/// A portable, machine-readable statement of the strict runtime contract.
/// The certificate deliberately describes only devices that the topology
/// runner constructs itself; it does not infer guarantees from a host setup.
#[derive(Serialize)]
struct RuntimeCertificate {
    format: &'static str,
    status: &'static str,
    profile: RuntimeSupportProfile,
    source: CertificateSource,
    repeatability: CertificateRepeatability,
    services: BTreeMap<String, CertificateServiceEvidence>,
}

#[derive(Serialize)]
struct RuntimeSupportProfile {
    id: &'static str,
    architecture: &'static str,
    execution: &'static str,
    virtual_time: &'static str,
    entropy: &'static str,
    network: &'static str,
    storage: &'static str,
    host_fds: &'static str,
    rejected: Vec<&'static str>,
    known_limit: &'static str,
}

#[derive(Serialize)]
struct CertificateSource {
    plan_sha256: String,
    plan: String,
    /// Exact UTF-8 bytes hashed by `plan_sha256`. Keeping the normalized plan
    /// in the certificate makes the fixed-plan witness independently
    /// inspectable after it leaves the execution directory.
    plan_contents: String,
}

#[derive(Serialize)]
struct CertificateRepeatability {
    executions: u8,
    comparison: &'static str,
    evidence_sha256: String,
}

#[derive(Serialize)]
struct CertificateServiceEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_start: Option<starting_state::ExecutionStart>,
    entropy_probe_sha256: String,
    serial_sha256: Vec<String>,
    storage_sha256: BTreeMap<String, String>,
    network_traffic: BTreeMap<String, NetworkTraffic>,
    virtual_time_ns: Vec<u64>,
    execution_ledgers: Vec<ExecutionLedgerEvidence>,
    machine_execution_ledger: ExecutionLedgerEvidence,
    machine_execution_trace_decisions: usize,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
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
    rounds: u64,
    #[serde(default)]
    max_rounds: u64,
    #[serde(default)]
    lifecycle_barrier_rounds: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    barrier_rounds: Option<u64>,
}

/// Deterministic CPU throttling: while active, the service is pumped only on
/// global rounds that satisfy the modulus, so one throttle round is a real
/// vCPU slice and the skipped rounds are simply not pumped.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct CpuThrottleState {
    until_round: u64,
    every_n_rounds: u32,
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
    /// One immutable memory image in a memfd. Every restored child maps this
    /// file MAP_PRIVATE, so the kernel shares untouched pages and isolates
    /// writes with COW instead of copying a snapshot file per sibling.
    branch: Arc<BranchPoint>,
    memory_bytes: u64,
    networks: BTreeMap<String, SimNetState>,
}

#[derive(Clone)]
struct ServiceSchedulerCheckpoint {
    serial_contents: Vec<Vec<u8>>,
    serial_pending_bytes: usize,
    program_counters: Vec<u64>,
    next_fault: usize,
    paused_until: Option<u64>,
    throttle: Option<CpuThrottleState>,
    faults: Vec<AppliedFault>,
    network_traffic: BTreeMap<String, NetworkTraffic>,
    network_trace: BTreeMap<String, Vec<NetworkFrame>>,
    storage_sha256: BTreeMap<String, String>,
    virtual_time_ns: Option<Vec<u64>>,
    private_dirty_pages: Option<u64>,
    /// Runtime execution samples inherited with a COW branch. They are not
    /// Firecracker snapshot state: this campaign tree owns their lifetime.
    execution_locations: Option<Vec<Vec<u64>>>,
    /// Runtime-only rolling state inherited by every COW child.
    execution_ledgers: Option<Vec<ExecutionLedger>>,
    /// VM-global rolling execution state inherited by restored children.
    machine_execution_state: Option<MachineExecutionState>,
    devices: Option<vmm::checkpoint::ExecutionDeviceState>,
}

#[derive(Clone)]
struct CampaignCheckpoint {
    switches: BTreeMap<String, SimSwitchState>,
    services: BTreeMap<String, ServiceVmCheckpoint>,
    scheduler: BTreeMap<String, ServiceSchedulerCheckpoint>,
    round: u64,
}

/// One materialized node in a campaign operation-prefix tree. VMM state lives
/// only in retained immutable memfds and is deliberately absent from result
/// JSON. Dropping this tree drops every retained branch; the locked replay
/// bundle remains a normal, self-contained event plan.
#[derive(Clone)]
struct CampaignPrefixCheckpoint {
    checkpoint: CampaignCheckpoint,
    actions: Vec<AppliedCampaignAction>,
    events: Vec<CampaignEvent>,
    barriers: Vec<CampaignUartBarrier>,
    boundaries: Vec<CampaignCheckpointBoundary>,
}

#[derive(Clone)]
struct CampaignCheckpointBoundary {
    actions: Vec<AppliedCampaignAction>,
    round: u64,
    markers: Vec<String>,
    application_blocks: BTreeMap<String, Vec<ApplicationBlock>>,
    thread_scheduling: BTreeMap<String, Vec<ThreadSchedulingDecision>>,
    thread_synchronization: BTreeMap<String, Vec<ThreadSynchronizationEvent>>,
    structured_choices: BTreeMap<String, Vec<StructuredChoiceDecision>>,
    program_counters: BTreeMap<String, Vec<String>>,
    serial_sha256: BTreeMap<String, String>,
    serial_contents: BTreeMap<String, Vec<u8>>,
    serial_pending_bytes: BTreeMap<String, usize>,
    network_traffic: BTreeMap<String, BTreeMap<String, NetworkTraffic>>,
    storage_sha256: BTreeMap<String, BTreeMap<String, String>>,
    virtual_time_ns: BTreeMap<String, Vec<u64>>,
    execution_ledgers: BTreeMap<String, Vec<ExecutionLedgerEvidence>>,
    machine_execution_ledgers: BTreeMap<String, ExecutionLedgerEvidence>,
}

enum CampaignPrefixResult {
    Ready(CampaignPrefixCheckpoint),
    MarkerGuardRejected,
    SerialGuardRejected,
}

/// Materialize operation/action prefixes within a bounded LRU cache, then
/// fork every leaf from its nearest retained ancestor. This is a real tree
/// rather than a cache keyed only by operation names: the key includes the
/// exact serial input and barrier actions, so a faulted prefix never leaks into
/// an ordinary sibling.
struct CampaignCheckpointTree {
    root: CampaignCheckpoint,
    prefixes: BTreeMap<String, CampaignPrefixCheckpoint>,
    reuses: usize,
    prefix_captures: usize,
    prefix_restores: usize,
    prefix_cow_restore_bytes: u64,
    retained_memory_bytes: u64,
    retained_private_dirty_pages: u64,
    cache_policy: checkpoint_cache::Policy,
    prefix_evictions: usize,
}

impl CampaignCheckpoint {
    fn memory_bytes(&self) -> u64 {
        self.services
            .values()
            .map(|service| service.memory_bytes)
            .sum()
    }

    fn private_dirty_pages(&self) -> u64 {
        self.scheduler
            .values()
            .filter_map(|service| service.private_dirty_pages)
            .sum()
    }
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

    fn serial_input_depth(&self) -> Result<usize, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .serial_input_depth()
            .map_err(|error| error.to_string())
    }

    fn serial_input_diagnostics(&self) -> Result<String, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .serial_input_diagnostics()
            .map_err(|error| error.to_string())
    }

    fn jump_virtual_time(&self, nanoseconds: i64) -> Result<(), String> {
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

    /// Fingerprint the next guest-visible entropy bytes without consuming
    /// them. This closes the gap between a configured seed and evidence that
    /// the live VMM restored the same seeded stream.
    fn entropy_probe_sha256(&self) -> String {
        let probe = self
            .vmm
            .lock()
            .expect("VMM lock poisoned")
            .entropy_probe(64);
        format!("{:x}", Sha256::digest(probe))
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

    fn set_network_link_conditions(
        &self,
        network: &str,
        destination: &str,
        action: Option<&CampaignAction>,
    ) -> Result<(), String> {
        let (_, id) = self
            .networks
            .iter()
            .find(|(name, _)| name == network)
            .ok_or_else(|| format!("service is not on network: {network}"))?;
        let conditions = action.map(|action| SimNetConfig {
            drop_ppm: action.drop_ppm.unwrap_or(0),
            duplicate_ppm: action.duplicate_ppm.unwrap_or(0),
            corrupt_ppm: action.corrupt_ppm.unwrap_or(0),
            latency_rounds: action.latency_rounds.unwrap_or(0),
            jitter_rounds: action.jitter_rounds.unwrap_or(0),
            tx_bytes_per_round: action.tx_bytes_per_round.unwrap_or(0),
            mtu_bytes: action.mtu_bytes.unwrap_or(0),
            tx_queue_frames: action.tx_queue_frames.unwrap_or(0),
            rx_queue_frames: action.rx_queue_frames.unwrap_or(0),
            ..SimNetConfig::default()
        });
        let vmm = self.vmm.lock().expect("VMM lock poisoned");
        if !vmm
            .with_simulated_network(id, |net| {
                net.set_simulated_link_conditions(destination, conditions)
            })
            .unwrap_or(false)
        {
            return Err(format!(
                "network does not have a simulated topology link: {network}"
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

    fn snapshot(&mut self, seed: u64) -> Result<ServiceVmCheckpoint, String> {
        let networks = self.save_network_states()?;
        let mut vmm = self.vmm.lock().expect("VMM lock poisoned");
        let vm_info = VmInfo::from(&*vmm);
        let branch = Arc::new(
            BranchPoint::capture(&mut vmm, &vm_info, seed).map_err(|error| error.to_string())?,
        );
        Ok(ServiceVmCheckpoint {
            memory_bytes: branch.mem_size(),
            branch,
            networks,
        })
    }

    fn dirty_page_count(&self) -> Option<u64> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .dirty_page_count()
    }

    fn execution_locations(&self) -> Result<Vec<Vec<u64>>, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .execution_location_samples()
            .map_err(|error| error.to_string())
    }

    fn execution_ledgers(&self) -> Result<Vec<ExecutionLedger>, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .execution_ledgers()
            .map_err(|error| error.to_string())
    }

    fn execution_ledger_evidence(&self) -> Result<Vec<ExecutionLedgerEvidence>, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .execution_ledger_evidence()
            .map_err(|error| error.to_string())
    }

    fn machine_execution_ledger_evidence(&self) -> Result<ExecutionLedgerEvidence, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .machine_execution_ledger_evidence()
            .map_err(|error| error.to_string())
    }

    fn machine_execution_state(&self) -> Result<MachineExecutionState, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .machine_execution_state()
            .map_err(|error| error.to_string())
    }

    fn machine_execution_trace(&self) -> Result<Vec<String>, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .machine_execution_trace()
            .map_err(|error| error.to_string())
    }

    fn enforce_machine_execution_trace(&self, trace: Vec<String>) -> Result<(), String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .enforce_machine_execution_trace(trace)
            .map_err(|error| error.to_string())
    }

    fn enforce_machine_execution_control_trace(&self, trace: Vec<String>) -> Result<(), String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .enforce_machine_execution_control_trace(trace)
            .map_err(|error| error.to_string())
    }

    fn machine_execution_replay_error(&self) -> Result<Option<String>, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .machine_execution_replay_error()
            .map_err(|error| error.to_string())
    }

    fn machine_execution_replay_divergence(&self) -> Result<Option<String>, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .machine_execution_replay_divergence()
            .map_err(|error| error.to_string())
    }

    fn machine_execution_replay_position(&self) -> Result<usize, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .machine_execution_replay_position()
            .map_err(|error| error.to_string())
    }

    fn wait_for_machine_execution_progress(&self, position: usize) -> Result<bool, String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .wait_for_machine_execution_progress(position, Duration::from_millis(1))
            .map_err(|error| error.to_string())
    }

    fn validate_execution_locations(&self) -> Result<(), String> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .validate_execution_location_samples()
            .map_err(|error| error.to_string())
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
    throttle: Option<CpuThrottleState>,
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
    topology: &TopologyPlan,
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
            service.vm.validate_execution_locations()?;
            let serial_contents = service
                .serial_logs
                .iter()
                .map(|path| {
                    fs::read(path)
                        .map_err(|error| format!("cannot checkpoint {}: {error}", path.display()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let storage_sha256 = service
                .vm
                .storage_fingerprints(&topology.services[name].run.storage)?;
            let virtual_time_ns = service.vm.virtual_time_ns()?;
            let execution_locations = service.vm.execution_locations()?;
            let execution_ledgers = service.vm.execution_ledgers()?;
            snapshots.insert(
                name.clone(),
                service.vm.snapshot(topology.services[name].run.run.seed)?,
            );
            // Saving devices can enqueue interrupts. Capture the execution
            // queue and transient devices AFTER save_state, at the same cut.
            let machine_execution_state = service.vm.machine_execution_state()?;
            let devices = service
                .vm
                .vmm
                .lock()
                .expect("VMM lock poisoned")
                .execution_device_state()
                .map_err(|error| error.to_string())?;
            scheduler.insert(
                name.clone(),
                ServiceSchedulerCheckpoint {
                    serial_contents,
                    serial_pending_bytes: service.vm.serial_input_depth()?,
                    program_counters: service.vm.paused_program_counters()?,
                    next_fault: service.next_fault,
                    paused_until: service.paused_until,
                    throttle: service.throttle.clone(),
                    faults: service.faults.clone(),
                    network_traffic: service.network_traffic.clone(),
                    network_trace: service.network_trace.clone(),
                    storage_sha256,
                    virtual_time_ns,
                    private_dirty_pages: service.vm.dirty_page_count(),
                    execution_locations: Some(execution_locations),
                    execution_ledgers: Some(execution_ledgers),
                    machine_execution_state: Some(machine_execution_state),
                    devices: Some(devices),
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
        let retained_memory_bytes = root.memory_bytes();
        let retained_private_dirty_pages = root.private_dirty_pages();
        Self {
            root,
            prefixes: BTreeMap::new(),
            reuses: 0,
            prefix_captures: 0,
            prefix_restores: 0,
            prefix_cow_restore_bytes: 0,
            retained_memory_bytes,
            retained_private_dirty_pages,
            cache_policy: checkpoint_cache::Policy::new(checkpoint_cache::PREFIX_MEMORY_BUDGET),
            prefix_evictions: 0,
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
        expected: Option<&BTreeMap<String, Vec<String>>>,
    ) -> Result<CampaignPrefixResult, String> {
        let mut prefix = Vec::new();
        let mut parent = CampaignPrefixCheckpoint {
            checkpoint: self.root.clone(),
            actions: Vec::new(),
            events: Vec::new(),
            barriers: Vec::new(),
            boundaries: Vec::new(),
        };
        for (index, operation) in schedule.operations.iter().enumerate() {
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
            let event = match campaign_schedule_event(campaign, schedule, index, &parent.checkpoint)
            {
                Ok(event) => event,
                // A template capture is a dynamic serial precondition. A
                // history without its value is simply not an executable leaf.
                Err(_) => return Ok(CampaignPrefixResult::SerialGuardRejected),
            };
            prefix.push(event);
            let key = campaign_prefix_key(&prefix)?;
            if let Some(existing) = self.prefixes.get(&key) {
                if let Some(expected) = expected {
                    starting_state::check_prefix(&existing.checkpoint, expected)?;
                }
                self.reuses += 1;
                self.cache_policy.touch(&key);
                parent = existing.clone();
                continue;
            }
            let (checkpoint, applied, barrier) = checkpoint_campaign_operation(
                topology,
                &parent.checkpoint,
                &prefix[prefix.len() - 1],
                &directory.join("prefix-work").join(&key),
                expected,
            )?;
            self.prefix_restores += 1;
            self.prefix_captures += 1;
            self.prefix_cow_restore_bytes = self
                .prefix_cow_restore_bytes
                .saturating_add(parent.checkpoint.memory_bytes());
            self.retained_private_dirty_pages = self
                .retained_private_dirty_pages
                .saturating_add(checkpoint.private_dirty_pages());
            let mut actions = parent.actions.clone();
            actions.extend(applied.clone());
            let mut boundaries = parent.boundaries.clone();
            boundaries.push(campaign_checkpoint_boundary(&checkpoint, applied));
            let mut barriers = parent.barriers.clone();
            barriers.push(barrier);
            parent = CampaignPrefixCheckpoint {
                checkpoint,
                actions,
                events: prefix.clone(),
                barriers,
                boundaries,
            };
            let bytes = parent.checkpoint.memory_bytes();
            let (admitted, evicted) = self.cache_policy.admit(key.clone(), bytes);
            for key in evicted {
                if let Some(removed) = self.prefixes.remove(&key) {
                    self.retained_memory_bytes -= removed.checkpoint.memory_bytes();
                    self.prefix_evictions += 1;
                }
            }
            if admitted {
                self.retained_memory_bytes = self.retained_memory_bytes.saturating_add(bytes);
                self.prefixes.insert(key, parent.clone());
            }
        }
        Ok(CampaignPrefixResult::Ready(parent))
    }

    fn nodes(&self) -> usize {
        // Include the boot/readiness root, which is also a reusable snapshot.
        self.prefixes.len() + 1
    }

    fn root_boundary(&self) -> CampaignCheckpointBoundary {
        campaign_checkpoint_boundary(&self.root, Vec::new())
    }

    fn economics(
        &self,
        leaf_restores: usize,
        leaf_cow_restore_bytes: u64,
    ) -> CampaignCheckpointEconomics {
        CampaignCheckpointEconomics {
            root_captures: 1,
            prefix_captures: self.prefix_captures,
            checkpoint_nodes: self.nodes(),
            prefix_reuses: self.reuses,
            prefix_restores: self.prefix_restores,
            leaf_restores,
            topology_restores: self.prefix_restores.saturating_add(leaf_restores),
            avoided_prefix_recomputations: self.reuses,
            retained_memory_bytes: self.retained_memory_bytes,
            shared_cow_restore_bytes: self
                .prefix_cow_restore_bytes
                .saturating_add(leaf_cow_restore_bytes),
            private_dirty_pages: self.retained_private_dirty_pages,
            snapshot_file_bytes: 0,
            prefix_evictions: self.prefix_evictions,
        }
    }
}

fn campaign_prefix_key(events: &[CampaignEvent]) -> Result<String, String> {
    let encoded = serde_json::to_vec(events)
        .map_err(|error| format!("cannot encode campaign checkpoint prefix: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

/// Restore a parent topology branch just long enough to execute one service
/// operation. Its UART checkpoint is the fork barrier. We then stop every
/// vCPU and capture the complete resulting topology, so sibling operations
/// begin from byte-identical VM, disk, network, serial, and scheduler state.
fn checkpoint_campaign_operation(
    topology: &TopologyPlan,
    parent: &CampaignCheckpoint,
    event: &CampaignEvent,
    directory: &Path,
    expected: Option<&BTreeMap<String, Vec<String>>>,
) -> Result<
    (
        CampaignCheckpoint,
        Vec<AppliedCampaignAction>,
        CampaignUartBarrier,
    ),
    String,
> {
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
            service_initramfs(service)?,
            serial,
            &switches,
            scheduler.execution_locations.as_deref(),
            scheduler.execution_ledgers.as_deref(),
            scheduler.machine_execution_state.as_ref(),
            scheduler.devices.as_ref(),
            parent
                .services
                .get(name)
                .ok_or_else(|| format!("checkpoint is missing VM state for {name}"))?,
        )?;
        if let Some(expected) = expected {
            vm.enforce_machine_execution_control_trace(
                expected
                    .get(name)
                    .ok_or_else(|| format!("missing replay stream for {name}"))?
                    .clone(),
            )?;
        }
        services.insert(
            name.clone(),
            ServiceRuntime {
                vm,
                serial_logs,
                next_fault: scheduler.next_fault,
                paused_until: scheduler.paused_until,
                throttle: scheduler.throttle.clone(),
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
    let mut round = parent.round;
    terminate_campaign_shell_processes(event, topology, &mut services, &switches, &mut round)?;
    let mut applied =
        recover_campaign_faults(event, topology, &mut services, &switches, &mut round)?;
    let mut target = services
        .remove(&event.service)
        .ok_or_else(|| format!("campaign operation service disappeared: {}", event.service))?;
    let serial = target.serial_logs[0].clone();
    let injection = inject_campaign_operation(
        &event.service,
        &mut target,
        &serial,
        topology,
        &mut services,
        &switches,
        &event.event,
        &mut round,
        &mut applied,
    );
    services.insert(event.service.clone(), target);
    reject_active_replay_divergence(&services)?;
    let barrier = injection?;
    let checkpoint =
        capture_campaign_checkpoint(directory, topology, &mut services, &switches, round)?;
    Ok((checkpoint, applied, barrier))
}

fn recover_campaign_faults(
    event: &CampaignEvent,
    topology: &TopologyPlan,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    round: &mut u64,
) -> Result<Vec<AppliedCampaignAction>, String> {
    if event.recover_faults.is_empty() {
        return Ok(Vec::new());
    }
    let mut driver = services
        .remove(&event.service)
        .ok_or_else(|| format!("campaign operation service disappeared: {}", event.service))?;
    let mut applied = Vec::with_capacity(event.recover_faults.len());
    let result = event.recover_faults.iter().try_for_each(|action| {
        applied.push(apply_campaign_action(
            action,
            &event.service,
            &mut driver,
            topology,
            services,
            switches,
            round,
        )?);
        Ok::<(), String>(())
    });
    services.insert(event.service.clone(), driver);
    result.map(|()| applied)
}

fn terminate_campaign_shell_processes(
    event: &CampaignEvent,
    topology: &TopologyPlan,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    round: &mut u64,
) -> Result<(), String> {
    let mut service_names = event.terminate_shell_processes.clone();
    if !event.recover_faults.is_empty() && !service_names.contains(&event.service) {
        service_names.push(event.service.clone());
        service_names.sort();
    }
    for service_name in &service_names {
        let termination = campaign_shell_termination_event(service_name);
        let mut service = services
            .remove(service_name)
            .ok_or_else(|| format!("eventually termination service disappeared: {service_name}"))?;
        let serial = service.serial_logs[0].clone();
        let result = inject_campaign_operation(
            service_name,
            &mut service,
            &serial,
            topology,
            services,
            switches,
            &termination,
            round,
            &mut Vec::new(),
        );
        services.insert(service_name.clone(), service);
        result?;
    }
    Ok(())
}

fn campaign_shell_termination_event(service: &str) -> EventPlan {
    let name = format!("terminate_{service}");
    let command = serde_json::to_string(&serde_json::json!({"name": &name}))
        .expect("termination command serializes");
    EventPlan {
        data_hex: hex(format!("THES:SHELL:terminate:{command}\n").as_bytes()),
        checkpoint: Some(format!("THES:CHECKPOINT:{name}")),
        actions: Vec::new(),
    }
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
    if let [command, flag_plan, plan, flag_output, output] = args.as_slice() {
        if command == "certify" {
            if flag_plan != "--plan" || flag_output != "--output" {
                return Err(USAGE.to_owned());
            }
            return certify(plan, Path::new(output));
        }
    }
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
    execute_plan(topology, Path::new(plan), PathBuf::from(output), minimize)
}

/// Execute a plan or a locked replay plan. Keeping this boundary shared with
/// certification means the second certification run exercises the exact same
/// replay checks as an operator's normal replay command.
fn execute_plan(
    mut topology: TopologyPlan,
    plan: &Path,
    output: PathBuf,
    minimize: bool,
) -> Result<(), String> {
    if topology.format != "theseus-compose-plan-v1" || topology.services.is_empty() {
        return Err("unsupported or empty topology plan".to_owned());
    }
    resolve_topology_artifacts(&mut topology, plan)?;
    let service_names = topology.services.keys().cloned().collect::<Vec<_>>();
    if topology.starting_checkpoint.is_some()
        && topology.replay_start != starting_state::ReplayStart::ReadyCheckpoint
    {
        return Err("a retained topology checkpoint cannot be replayed as fresh_boot".to_owned());
    }
    if topology.replay_start == starting_state::ReplayStart::ReadyCheckpoint {
        validate_certification_plan_devices(&topology)?;
    }
    let recorded_campaign = recorded_campaign_result(plan)?;
    if recorded_campaign
        .as_ref()
        .and_then(|result| result.starting_checkpoint_sha256.as_ref())
        != topology
            .starting_checkpoint
            .as_ref()
            .map(|artifact| &artifact.sha256)
        && recorded_campaign.is_some()
    {
        return Err("recorded campaign checkpoint origin differs from its replay plan".to_owned());
    }
    let (
        expected_serial,
        expected_faults,
        expected_network,
        expected_actions,
        expected_storage,
        expected_traffic,
        expected_entropy,
        expected_virtual_time,
        expected_execution_ledgers,
        expected_machine_execution_ledgers,
        expected_machine_execution_traces,
        expected_lifecycle_rounds,
    ) = if recorded_campaign.is_some() {
        (
            None, None, None, None, None, None, None, None, None, None, None, None,
        )
    } else if topology.machine_replay == MachineReplayMode::HostInputs {
        // Portable campaign exports promise the recorded external inputs and
        // declared properties, not byte-identical Linux execution between
        // those inputs. Keep comparisons for host-controlled topology effects
        // and use the complete trace only for its host-input projection.
        (
            None,
            recorded_fault_fingerprints(plan, &service_names)?,
            recorded_network_fingerprint(plan)?,
            recorded_campaign_actions(plan)?,
            None,
            None,
            None,
            None,
            None,
            None,
            recorded_machine_execution_traces(plan, &service_names)?,
            recorded_lifecycle_barrier_rounds(plan)?,
        )
    } else {
        (
            recorded_serial_fingerprints(plan, &service_names)?,
            recorded_fault_fingerprints(plan, &service_names)?,
            recorded_network_fingerprint(plan)?,
            recorded_campaign_actions(plan)?,
            recorded_storage_fingerprints(plan, &service_names)?,
            recorded_network_traffic(plan, &service_names)?,
            recorded_entropy_probes(plan, &service_names)?,
            recorded_virtual_times(plan, &service_names)?,
            recorded_execution_ledgers(plan, &service_names)?,
            recorded_machine_execution_ledgers(plan, &service_names)?,
            recorded_machine_execution_traces(plan, &service_names)?,
            recorded_lifecycle_barrier_rounds(plan)?,
        )
    };
    let retained_root = topology
        .starting_checkpoint
        .as_ref()
        .map(|locked| starting_state::load(&topology, locked))
        .transpose()?;
    if plan.file_name().and_then(|name| name.to_str()) == Some("replay-plan.json") {
        if topology.replay_start == starting_state::ReplayStart::ReadyCheckpoint
            && retained_root.is_none()
        {
            return Err("ready-checkpoint replay is missing its retained root".to_owned());
        }
        if recorded_campaign.is_none() {
            starting_state::check_recorded_contract(&topology, plan)?;
            if retained_root.is_some() && expected_machine_execution_traces.is_none() {
                return Err("checkpoint replay is missing its active machine stream".to_owned());
            }
        }
    }
    if let (Some(root), Some(expected)) = (&retained_root, &expected_machine_execution_traces) {
        starting_state::check_prefix(root, expected)?;
        starting_state::check_recorded_origin(&topology, root, plan)?;
    }
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
            || expected_entropy.is_some()
            || expected_virtual_time.is_some()
            || expected_execution_ledgers.is_some()
            || expected_machine_execution_ledgers.is_some()
            || expected_machine_execution_traces.is_some()
            || expected_lifecycle_rounds.is_some()
        {
            return Err(
                "campaign bundles replay their recorded schedules, not single-run fingerprints"
                    .to_owned(),
            );
        }
        if minimize {
            execute_campaign_minimized(topology, &output, Path::new(plan), retained_root)
        } else {
            execute_campaign(topology, &output, recorded_campaign.as_ref(), retained_root)
        }
    } else {
        if minimize {
            return Err("--minimize requires a campaign replay bundle".to_owned());
        }
        let root = if topology.replay_start == starting_state::ReplayStart::ReadyCheckpoint {
            Some(starting_state::boot_or_load(
                &mut topology,
                &output.join("checkpoint"),
                "",
                retained_root,
            )?)
        } else {
            None
        };
        execute(
            topology,
            &output,
            root.as_ref(),
            ExecutionCompletion::GuestExit,
            expected_serial,
            expected_faults,
            expected_network,
            expected_actions,
            expected_storage,
            expected_traffic,
            expected_entropy,
            expected_virtual_time,
            expected_execution_ledgers,
            expected_machine_execution_ledgers,
            expected_machine_execution_traces,
            expected_lifecycle_rounds,
        )
    }
}

/// Run a strict deterministic topology twice on real KVM. The first run
/// locks its artifacts and fingerprints; the second is an ordinary Theseus
/// replay and therefore fails on changed serial, entropy, storage, network,
/// virtual-clock, lifecycle, or action evidence.
fn certify(plan: &str, output: &Path) -> Result<(), String> {
    if output.exists() {
        return Err(format!(
            "certificate output already exists: {}",
            output.display()
        ));
    }
    let input = fs::read(plan).map_err(|error| format!("cannot read {plan}: {error}"))?;
    let mut topology: TopologyPlan = serde_json::from_slice(&input)
        .map_err(|error| format!("cannot parse topology plan: {error}"))?;
    validate_certification_plan(&topology)?;
    if topology.replay_start == starting_state::ReplayStart::ReadyCheckpoint {
        let runner = env::current_exe().map_err(|error| error.to_string())?;
        topology.topology_runner = Some(Artifact {
            sha256: vmm::checkpoint::artifact(&runner, 256 * 1024 * 1024)
                .map_err(|error| error.to_string())?
                .sha256,
            path: runner.display().to_string(),
        });
    }
    ensure_kvm_access()?;

    fs::create_dir_all(output).map_err(|error| error.to_string())?;
    let first = output.join("first");
    execute_plan(topology, Path::new(plan), first.clone(), false)?;

    let replay_plan = first.join("replay-plan.json");
    let replay_input = fs::read(&replay_plan)
        .map_err(|error| format!("cannot read {}: {error}", replay_plan.display()))?;
    let replay_topology: TopologyPlan = serde_json::from_slice(&replay_input)
        .map_err(|error| format!("cannot parse {}: {error}", replay_plan.display()))?;
    let checkpoint_start = replay_topology.starting_checkpoint.is_some();
    let replay = output.join("replay");
    execute_plan(replay_topology, &replay_plan, replay, false)?;

    let services = certification_service_evidence(&first)?;
    let certificate = RuntimeCertificate {
        format: if checkpoint_start { "theseus-runtime-certificate-v5" } else { "theseus-runtime-certificate-v4" },
        status: "passed",
        profile: RuntimeSupportProfile {
            id: "linux-kvm-simulated-io-v1",
            architecture: runtime_architecture()?,
            execution: if checkpoint_start {
                "two real-KVM executions restored from one locked whole-topology ready checkpoint; boot is an inherited prefix, and the second resumed suffix is actively gated by the first exact machine trace"
            } else {
                "two fresh-boot real-KVM executions with per-vCPU ledgers; the second is actively gated by the first run's exact machine-wide trace of KVM exits and host inputs"
            },
            virtual_time: "exit-counted quanta with exact final vCPU-clock fingerprint equality",
            entropy: "seeded virtio-rng with exact next-64-byte fingerprint equality",
            network: "Theseus simulated virtio-net only",
            storage: "Theseus in-memory simulated virtio-block only",
            host_fds: "no guest-visible host-backed network, vsock, block, or pmem device",
            rejected: vec![
                "host timerfd rate limiters",
                "tap network devices",
                "Unix-socket vsock devices",
                "file-backed or vhost-user block devices",
                "host-backed persistent memory",
            ],
            known_limit: "counter reads can free-run within an exit-counted quantum; this profile compares end-of-run fingerprints, not instruction-by-instruction clock reads",
        },
        source: CertificateSource {
            plan_sha256: format!("{:x}", Sha256::digest(if checkpoint_start { &replay_input } else { &input })),
            plan: plan.to_owned(),
            plan_contents: String::from_utf8(if checkpoint_start { replay_input.clone() } else { input.clone() })
                .expect("a parsed JSON plan is valid UTF-8"),
        },
        repeatability: CertificateRepeatability {
            executions: 2,
            comparison: "the replay compares ordered KVM exits, explicit host inputs, serial, entropy, storage, network traffic, virtual clocks, lifecycle rounds, and scheduled actions exactly",
            evidence_sha256: certification_evidence_sha256(&first, &services)?,
        },
        services,
    };
    let path = output.join("certificate.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&certificate).expect("runtime certificate serializes"),
    )
    .map_err(|error| error.to_string())?;
    println!("certified: {}", path.display());
    Ok(())
}

fn validate_certification_plan(topology: &TopologyPlan) -> Result<(), String> {
    if topology.format != "theseus-compose-plan-v1" || topology.services.is_empty() {
        return Err("certification requires a non-empty Theseus Compose plan".to_owned());
    }
    if topology.campaign.is_some() {
        return Err(
            "certification requires one fixed topology schedule, not an autonomous campaign"
                .to_owned(),
        );
    }
    for (name, service) in &topology.services {
        let Some(clock) = &service.run.run.virtual_time else {
            return Err(format!(
                "service {name:?} has no virtual-time configuration; deterministic certification fails closed"
            ));
        };
        if clock.tick_ns == 0 || clock.exits_per_tick == 0 {
            return Err(format!(
                "service {name:?} has an invalid virtual-time configuration; deterministic certification fails closed"
            ));
        }
    }
    Ok(())
}

fn validate_certification_plan_devices(topology: &TopologyPlan) -> Result<(), String> {
    for (name, service) in &topology.services {
        if service
            .run
            .run
            .virtual_time
            .as_ref()
            .is_none_or(|clock| clock.tick_ns == 0 || clock.exits_per_tick == 0)
            || service.run.run.vcpu_count == 0
            || service.run.run.vcpu_count > 32
            || service.run.run.mem_size_mib == 0
            || service.run.run.mem_size_mib > 65536
        {
            return Err(format!(
                "invalid deterministic checkpoint machine configuration for {name:?}"
            ));
        }
    }
    Ok(())
}

fn ensure_kvm_access() -> Result<(), String> {
    let kvm = Path::new("/dev/kvm");
    let metadata = fs::metadata(kvm).map_err(|_| {
        "real KVM is required for certification: /dev/kvm is unavailable".to_owned()
    })?;
    if !metadata.file_type().is_char_device() {
        return Err(
            "real KVM is required for certification: /dev/kvm is not a character device".to_owned(),
        );
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(kvm)
        .map_err(|_| {
            "real KVM is required for certification: /dev/kvm is not readable and writable"
                .to_owned()
        })?;
    Ok(())
}

fn runtime_architecture() -> Result<&'static str, String> {
    match env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        architecture => Err(format!(
            "deterministic certification is supported only on amd64 or arm64, not {architecture}"
        )),
    }
}

fn certification_service_evidence(
    first: &Path,
) -> Result<BTreeMap<String, CertificateServiceEvidence>, String> {
    let plan: TopologyPlan = serde_json::from_slice(
        &fs::read(first.join("replay-plan.json"))
            .map_err(|error| format!("cannot read locked certification plan: {error}"))?,
    )
    .map_err(|error| format!("cannot parse locked certification plan: {error}"))?;
    plan.services
        .keys()
        .map(|name| {
            let path = first.join("services").join(name).join("result.json");
            let recorded: RecordedServiceResult = serde_json::from_slice(
                &fs::read(&path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
            )
            .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
            let entropy_probe_sha256 = recorded.entropy_probe_sha256.ok_or_else(|| {
                format!(
                    "{} has no entropy probe; certification fails closed",
                    path.display()
                )
            })?;
            let virtual_time_ns = recorded.virtual_time_ns.flatten().ok_or_else(|| {
                format!(
                    "{} has no virtual-clock evidence; certification fails closed",
                    path.display()
                )
            })?;
            if virtual_time_ns.is_empty() {
                return Err(format!(
                    "{} has empty virtual-clock evidence; certification fails closed",
                    path.display()
                ));
            }
            let execution_ledgers = recorded.execution_ledgers.ok_or_else(|| {
                format!(
                    "{} has no ordered KVM execution ledger; certification fails closed",
                    path.display()
                )
            })?;
            if execution_ledgers.is_empty()
                || execution_ledgers
                    .iter()
                    .any(|ledger| ledger.decisions == 0 || ledger.sha256.len() != 64)
            {
                return Err(format!(
                    "{} has empty or malformed ordered KVM execution evidence; certification fails closed",
                    path.display()
                ));
            }
            let machine_execution_ledger = recorded.machine_execution_ledger.ok_or_else(|| {
                format!(
                    "{} has no machine-wide execution stream; certification fails closed",
                    path.display()
                )
            })?;
            if machine_execution_ledger.decisions == 0
                || machine_execution_ledger.sha256.len() != 64
                || machine_execution_ledger.tail.is_empty()
            {
                return Err(format!(
                    "{} has empty or malformed machine-wide execution evidence; certification fails closed",
                    path.display()
                ));
            }
            let machine_execution_trace = recorded.machine_execution_trace.ok_or_else(|| {
                format!(
                    "{} has no active machine execution replay trace; certification fails closed",
                    path.display()
                )
            })?;
            if machine_execution_trace.len()
                != usize::try_from(machine_execution_ledger.decisions).unwrap_or(usize::MAX)
            {
                return Err(format!(
                    "{} has inconsistent machine execution replay evidence; certification fails closed",
                    path.display()
                ));
            }
            Ok((
                name.clone(),
                CertificateServiceEvidence {
                    execution_start: recorded.execution_start,
                    entropy_probe_sha256,
                    serial_sha256: recorded.serial_sha256,
                    storage_sha256: recorded.storage_sha256.unwrap_or_default(),
                    network_traffic: recorded.network_traffic.unwrap_or_default(),
                    virtual_time_ns,
                    execution_ledgers,
                    machine_execution_ledger,
                    machine_execution_trace_decisions: machine_execution_trace.len(),
                },
            ))
        })
        .collect()
}

fn certification_evidence_sha256(
    first: &Path,
    services: &BTreeMap<String, CertificateServiceEvidence>,
) -> Result<String, String> {
    let mut evidence = fs::read(first.join("topology-result.json"))
        .map_err(|error| format!("cannot read topology certification evidence: {error}"))?;
    evidence.extend(
        serde_json::to_vec(services)
            .map_err(|error| format!("cannot encode certification evidence: {error}"))?,
    );
    Ok(format!("{:x}", Sha256::digest(evidence)))
}

/// Execute an autonomous campaign from one reusable, whole-topology branch
/// point. Each child restores every VM, simulated NIC/switch, UART transcript,
/// and scheduler cursor before its own operation history is injected.
/// One structured live-progress line for a completed campaign timeline.
/// Written to stderr while the exploration runs, so CI and wrappers can
/// follow the search without parsing the final result file.
fn campaign_progress_line(
    completed: usize,
    index: usize,
    status: &str,
    operations: &[String],
    faults: &[String],
    failed_properties: &[&str],
    checkpoint_reuses: u64,
) -> String {
    let mut line = format!(
        "{{\"format\":\"theseus-progress-v1\",\"completed\":{completed},\"index\":{index},\"status\":\"{status}\",\"operations\":[{}],\"faults\":[{}]",
        operations
            .iter()
            .map(|operation| format!("\"{operation}\""))
            .collect::<Vec<_>>()
            .join(","),
        faults
            .iter()
            .map(|fault| format!("\"{fault}\""))
            .collect::<Vec<_>>()
            .join(","),
    );
    if !failed_properties.is_empty() {
        line.push_str(&format!(
            ",\"failed_properties\":[{}]",
            failed_properties
                .iter()
                .map(|property| format!("\"{property}\""))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    line.push_str(&format!(
        ",\"checkpoint_reuses\":{checkpoint_reuses}}}"
    ));
    line
}

fn execute_campaign(
    mut topology: TopologyPlan,
    output: &Path,
    recorded: Option<&RecordedCampaignResult>,
    verified_root: Option<CampaignCheckpoint>,
) -> Result<(), String> {
    // Exported campaign and minimized-counterexample plans become ordinary
    // topologies, so retain their portable replay contract in the plan itself.
    topology.machine_replay = MachineReplayMode::HostInputs;
    let campaign = topology
        .campaign
        .take()
        .expect("campaign execution requires a campaign");
    validate_campaign_test_commands(&campaign)?;
    for operation in &campaign.operations {
        if let Some(exploration) = &operation.thread_schedule_exploration {
            if exploration.strategy != "runnable_prefixes"
                || exploration.max_choices == 0
                || exploration.max_choices > 128
                || exploration.max_variants < 2
                || exploration.max_variants > 256
            {
                return Err(format!(
                    "campaign operation {:?} has an invalid runnable-prefix exploration",
                    operation.name
                ));
            }
        }
    }
    if let Some(recorded) = recorded {
        verify_recorded_campaign_guidance(campaign.guidance, campaign.coverage, recorded)?;
    }
    let checkpoint = starting_state::boot_or_load(
        &mut topology,
        &output.join("checkpoint"),
        &campaign.driver,
        verified_root,
    )?;
    let instruction_symbolizer = CampaignInstructionSymbolizer::from_topology(&topology);
    let application_symbolizer = CampaignApplicationSymbolizer::from_topology(&topology);
    let base = serde_json::to_vec(&topology)
        .map_err(|error| format!("cannot encode campaign base plan: {error}"))?;
    let mut checkpoints = CampaignCheckpointTree::new(checkpoint);
    let mut schedules = campaign_schedules(&campaign);
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
    let mut seen_instruction_locations = std::collections::BTreeSet::new();
    let mut seen_checkpoint_pcs = std::collections::BTreeSet::new();
    let mut seen_application_blocks = std::collections::BTreeSet::new();
    let mut seen_application_edges = std::collections::BTreeSet::new();
    let mut seen_structured_choices = std::collections::BTreeSet::new();
    let mut seen_scheduling_decisions = std::collections::BTreeSet::new();
    let mut pending = (0..schedules.len()).collect::<Vec<_>>();
    let mut observations = Vec::new();
    let mut replay_mismatches = Vec::new();
    let mut marker_guard_rejections = 0_usize;
    let mut serial_guard_rejections = 0_usize;
    let mut leaf_cow_restore_bytes = 0_u64;
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
            let (pending_index, selection) = select_campaign_schedule(
                &schedules,
                &pending,
                &observations,
                campaign.guidance,
                campaign.coverage,
            );
            (
                schedules[pending.remove(pending_index)].clone(),
                selection,
                None,
            )
        };
        let guidance_ledger = CampaignGuidanceLedger::from_observations(&observations);
        let guidance_evidence = (campaign.guidance == CampaignGuidance::Posterior)
            .then(|| campaign_posterior_evidence(&campaign, &schedule, &observations));
        // Guards inspect each exact restored parent checkpoint. A fault after
        // an earlier operation is visible to the next operation's guard, just
        // as it is to the guest; impossible prefixes never become leaves.
        let prefix = match checkpoints.checkpoint_for_guarded_schedule(
            &topology,
            &campaign,
            &schedule,
            output,
            expected
                .map(|run| &run.machine_execution_traces)
                .filter(|traces| !traces.is_empty()),
        )? {
            CampaignPrefixResult::Ready(prefix) => prefix,
            rejection => {
                if expected.is_some() {
                    return Err(format!(
                        "recorded campaign history no longer satisfies its operation guards: {}",
                        schedule
                            .operations
                            .iter()
                            .map(|operation| campaign_operation_choice_name(&campaign, *operation))
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
        apply_campaign_schedule(&mut replay, &campaign, &schedule, &prefix.events)?;
        let mut run: TopologyPlan = serde_json::from_slice(
            &serde_json::to_vec(&replay)
                .map_err(|error| format!("cannot encode campaign replay plan: {error}"))?,
        )
        .map_err(|error| format!("cannot decode campaign tail plan: {error}"))?;
        clear_campaign_events(&mut run);
        let run_dir = output.join("runs").join(format!("{index:03}"));
        leaf_cow_restore_bytes =
            leaf_cow_restore_bytes.saturating_add(prefix.checkpoint.memory_bytes());
        let status = execute(
            run,
            &run_dir,
            Some(&prefix.checkpoint),
            ExecutionCompletion::CampaignCheckpoint,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            expected
                .map(|run| run.machine_execution_traces.clone())
                .filter(|traces| !traces.is_empty()),
            None,
        );
        write_replay_plan(&run_dir.join("replay-plan.json"), &replay)?;
        if run_dir.join("topology-result.json").exists() {
            write_campaign_prefix_actions(&run_dir, prefix.actions)?;
        }
        let markers = campaign_markers(&run_dir)?;
        let mut application_blocks = campaign_application_blocks(&run_dir)?;
        application_symbolizer.symbolize(&mut application_blocks);
        let thread_scheduling = campaign_thread_scheduling(&run_dir)?;
        let thread_synchronization = campaign_thread_synchronization(&run_dir)?;
        let structured_choices = campaign_structured_choices(&run_dir)?;
        let actions = campaign_actions(&run_dir)?;
        let program_counters = campaign_checkpoint_program_counters(&prefix.checkpoint);
        let execution_locations = campaign_checkpoint_execution_locations(&prefix.checkpoint);
        let execution_ledgers = campaign_result_ledgers(&run_dir, "execution_ledgers")?;
        let machine_execution_ledgers =
            campaign_result_ledgers(&run_dir, "machine_execution_ledger")?;
        let machine_execution_traces = campaign_machine_execution_traces(&run_dir)?;
        let instruction_locations = instruction_symbolizer.symbolize(&execution_locations);
        let timeline = campaign_operation_timeline(
            &campaign,
            &schedule,
            &prefix.events,
            &prefix.barriers,
            &prefix.boundaries,
            &checkpoints.root_boundary(),
            &instruction_symbolizer,
            &application_symbolizer,
        );
        verify_campaign_structured_choices(&campaign, &schedule, &timeline)?;
        let instruction_novelty = campaign_instruction_locations(&execution_locations)
            .into_iter()
            .filter(|location| seen_instruction_locations.insert(location.clone()))
            .collect::<Vec<_>>();
        let checkpoint_pc_novelty = campaign_instruction_locations(&program_counters)
            .into_iter()
            .filter(|location| seen_checkpoint_pcs.insert(location.clone()))
            .collect::<Vec<_>>();
        let application_block_novelty = campaign_application_block_ids(&application_blocks)
            .into_iter()
            .filter(|block| seen_application_blocks.insert(block.clone()))
            .collect::<Vec<_>>();
        seen_application_edges.extend(application_blocks.iter().flat_map(|(service, blocks)| {
            blocks
                .iter()
                .filter(|block| block.edge.is_some())
                .map(|block| campaign_application_block_id(service, block))
        }));
        let structured_choice_novelty = structured_choices
            .iter()
            .flat_map(|(service, decisions)| {
                decisions.iter().map(move |decision| {
                    format!(
                        "{service}:{}:{}:{}",
                        decision.name, decision.upper_exclusive, decision.selected
                    )
                })
            })
            .filter(|decision| seen_structured_choices.insert(decision.clone()))
            .count();
        let scheduling_novelty = thread_scheduling
            .iter()
            .flat_map(|(service, decisions)| {
                decisions.iter().map(move |decision| {
                    format!(
                        "{service}:{}:{}:{}:{}",
                        decision.build_sha256,
                        decision.point_offset,
                        decision.runnable_mask,
                        decision.selected_thread
                    )
                })
            })
            .filter(|decision| seen_scheduling_decisions.insert(decision.clone()))
            .count();
        let state_sha256 = campaign_topology_state_sha256(&run_dir, &program_counters)?;
        let state_novel = seen_topology_states.insert(state_sha256.clone());
        let novelty = markers
            .into_iter()
            .filter(|marker| seen_markers.insert(marker.clone()))
            .collect::<Vec<_>>();
        let failed = status.is_err();
        let property_witnesses = campaign_property_witnesses(&campaign, &run_dir);
        let decision_trace = campaign_decision_trace(&campaign, &schedule, &timeline);
        observations.push(CampaignGuidanceObservation {
            operations: schedule.operations.clone(),
            decision_prefix: (campaign.guidance == CampaignGuidance::Unified)
                .then(|| campaign_schedule_decision_prefix(&schedule))
                .unwrap_or_default(),
            novel_markers: novelty.len(),
            novel_instructions: instruction_novelty.len(),
            novel_checkpoint_pcs: checkpoint_pc_novelty.len(),
            novel_application_blocks: application_block_novelty.len(),
            novel_structured_choices: (campaign.guidance == CampaignGuidance::Unified)
                .then_some(structured_choice_novelty)
                .unwrap_or_default(),
            novel_scheduling_decisions: (campaign.guidance == CampaignGuidance::Unified)
                .then_some(scheduling_novelty)
                .unwrap_or_default(),
            novel_state: state_novel,
            failed,
            property_witnesses: property_witnesses.clone(),
        });
        let run = CampaignRun {
            index,
            test_template: campaign_schedule_test_template(&campaign, &schedule).map(str::to_owned),
            operations: schedule
                .operations
                .iter()
                .map(|operation| campaign_operation_choice_name(&campaign, *operation))
                .collect(),
            decision_trace,
            thread_schedule_prefixes: schedule
                .operations
                .iter()
                .any(|choice| {
                    campaign.operations[choice.operation]
                        .thread_schedule_exploration
                        .is_some()
                })
                .then(|| schedule.thread_schedule_prefixes.clone())
                .unwrap_or_default(),
            fault: (schedule.faults.len() == 1)
                .then(|| campaign_fault_name(&campaign.faults[schedule.faults[0]])),
            faults: campaign_fault_names(&campaign, &schedule.faults),
            actions,
            selection,
            guidance_ledger,
            guidance_evidence,
            property_witnesses,
            timeline,
            program_counters,
            instruction_locations,
            instruction_novelty,
            checkpoint_pc_novelty,
            application_blocks,
            application_block_novelty,
            thread_scheduling,
            thread_synchronization,
            structured_choices,
            execution_ledgers,
            machine_execution_ledgers,
            machine_execution_traces,
            state_sha256,
            state_novel,
            status: if failed { "failed" } else { "passed" },
            novelty,
        };
        extend_runnable_prefix_schedules(
            &campaign,
            &schedule,
            &run.timeline,
            &mut schedules,
            &mut pending,
        );
        if let Some(expected) = expected {
            let mismatches = if topology.machine_replay == MachineReplayMode::HostInputs {
                campaign_host_input_replay_mismatches(expected, &run)
            } else {
                campaign_replay_mismatches(expected, &run)
            };
            if !mismatches.is_empty() {
                replay_mismatches.push(format!("run {index}: {}", mismatches.join(", ")));
            }
        }
        if recorded.is_none() {
            let failed_properties: Vec<&str> = if run.status == "failed" {
                run.property_witnesses.iter().map(String::as_str).collect()
            } else {
                Vec::new()
            };
            eprintln!(
                "{}",
                campaign_progress_line(
                    runs.len(),
                    run.index,
                    &run.status,
                    &run.operations,
                    &run.faults,
                    &failed_properties,
                    checkpoints.reuses as u64,
                )
            );
        }
        runs.push(run);
    }
    if runs.is_empty() {
        return Err("campaign produced no schedules after marker guards".to_owned());
    }
    let properties = evaluate_campaign_properties(&campaign, output, &runs)?;
    let search = CampaignSearchEvidence::from_search(
        &checkpoints,
        runs.len(),
        leaf_cow_restore_bytes,
        &observations,
    );
    let passed = runs.iter().all(|run| run.status == "passed")
        && properties
            .iter()
            .all(|property| property.status == "passed");
    let search_matches = recorded
        .and_then(|recorded| recorded.search.as_ref())
        .is_none_or(|expected| {
            if topology.machine_replay == MachineReplayMode::HostInputs {
                campaign_host_input_search_matches(expected, &search)
            } else {
                expected == &search
            }
        });
    let replay_verified = recorded.is_none()
        || (replay_mismatches.is_empty()
            && search_matches
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
    resolve_topology_artifacts(&mut replay, &first_plan)?;
    let guidance = campaign.guidance;
    let coverage = campaign.coverage;
    replay.campaign = Some(campaign);
    write_replay_plan(&output.join("replay-plan.json"), &replay)?;
    fs::write(
        output.join("campaign-result.json"),
        serde_json::to_vec_pretty(&CampaignResult {
            starting_checkpoint_sha256: topology
                .starting_checkpoint
                .as_ref()
                .map(|artifact| artifact.sha256.clone()),
            format: "theseus-compose-campaign-result-v1",
            decision_trace_format: "theseus-campaign-decision-trace-v1",
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
            test_template: replay
                .campaign
                .as_ref()
                .expect("campaign remains in replay plan")
                .test_template
                .clone(),
            test_templates: replay
                .campaign
                .as_ref()
                .expect("campaign remains in replay plan")
                .test_templates
                .clone(),
            max_parallel_commands: replay.campaign.as_ref().and_then(|campaign| {
                (campaign.test_template.is_some() || !campaign.test_templates.is_empty())
                    .then_some(campaign.max_parallel_commands)
            }),
            guidance,
            coverage,
            checkpoint_nodes: checkpoints.nodes(),
            checkpoint_reuses: checkpoints.reuses,
            generated_candidates: schedules.len(),
            marker_guard_rejections,
            serial_guard_rejections,
            unique_topology_states: seen_topology_states.len(),
            unique_instruction_locations: seen_instruction_locations.len(),
            unique_application_blocks: seen_application_blocks.len(),
            unique_application_edges: seen_application_edges.len(),
            thread_scheduling_decisions: runs
                .iter()
                .flat_map(|run| run.thread_scheduling.values())
                .map(Vec::len)
                .sum(),
            thread_synchronization_events: runs
                .iter()
                .flat_map(|run| run.thread_synchronization.values())
                .map(Vec::len)
                .sum(),
            execution_decisions: runs
                .iter()
                .flat_map(|run| run.machine_execution_ledgers.values())
                .map(|ledger| ledger.decisions)
                .sum(),
            structured_choice_decisions: runs
                .iter()
                .flat_map(|run| run.structured_choices.values())
                .map(Vec::len)
                .sum(),
            search,
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
                    if !search_matches {
                        detail.push("campaign search evidence changed".to_owned());
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
    _driver: &str,
) -> Result<CampaignCheckpoint, String> {
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    configure_container_networks(topology)?;
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
        let service_dir = directory.join("services").join(name);
        fs::create_dir_all(service_dir.join("artifacts")).map_err(|error| error.to_string())?;
        let service = topology
            .services
            .get_mut(name)
            .expect("topology service missing");
        lock_service_inputs(&service_dir, service)?;
    }
    write_replay_plan(&directory.join("replay-plan.json"), topology)?;
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
            service_initramfs(service)?,
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
                throttle: None,
                faults: Vec::new(),
                network_traffic: BTreeMap::new(),
                network_trace: BTreeMap::new(),
            },
        );
    }
    let max_rounds = topology
        .services
        .values()
        .map(|service| service.run.run.max_rounds)
        .max()
        .unwrap_or_else(default_max_rounds);
    let startup_round =
        start_services_in_dependency_order(topology, &mut services, &switches, max_rounds)?;
    capture_campaign_checkpoint(directory, topology, &mut services, &switches, startup_round)
}

fn campaign_actions(run: &Path) -> Result<Vec<AppliedCampaignAction>, String> {
    let result_path = run.join("topology-result.json");
    let result = fs::read(&result_path)
        .map_err(|error| format!("cannot read {}: {error}", result_path.display()))?;
    serde_json::from_slice::<TopologyResult>(&result)
        .map(|result| result.actions)
        .map_err(|error| format!("cannot parse {}: {error}", result_path.display()))
}

fn campaign_result_ledgers<T: serde::de::DeserializeOwned>(
    run: &Path,
    field: &str,
) -> Result<BTreeMap<String, T>, String> {
    let mut values = BTreeMap::new();
    for service in fs::read_dir(run.join("services")).map_err(|error| error.to_string())? {
        let service = service.map_err(|error| error.to_string())?;
        if !service
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let path = service.path().join("result.json");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        values.insert(
            service.file_name().to_string_lossy().into_owned(),
            serde_json::from_value(value[field].clone()).map_err(|error| {
                format!("invalid complete {field} in {}: {error}", path.display())
            })?,
        );
    }
    Ok(values)
}

fn campaign_machine_execution_traces(run: &Path) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut traces = BTreeMap::new();
    for service in fs::read_dir(run.join("services")).map_err(|error| error.to_string())? {
        let service = service.map_err(|error| error.to_string())?;
        if !service
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let result_path = service.path().join("result.json");
        let result = fs::read(&result_path)
            .map_err(|error| format!("cannot read {}: {error}", result_path.display()))?;
        let recorded: RecordedServiceResult = serde_json::from_slice(&result)
            .map_err(|error| format!("cannot parse {}: {error}", result_path.display()))?;
        let trace = recorded.machine_execution_trace.ok_or_else(|| {
            format!(
                "service result has no machine execution trace: {}",
                result_path.display()
            )
        })?;
        traces.insert(service.file_name().to_string_lossy().into_owned(), trace);
    }
    Ok(traces)
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
    verified_root: Option<CampaignCheckpoint>,
) -> Result<(), String> {
    topology.machine_replay = MachineReplayMode::HostInputs;
    let campaign = topology
        .campaign
        .take()
        .expect("campaign minimization requires a campaign");
    validate_campaign_test_commands(&campaign)?;
    let source = source_plan
        .parent()
        .ok_or_else(|| format!("campaign plan has no parent: {}", source_plan.display()))?;
    let recorded: RecordedCampaignResult = serde_json::from_slice(
        &fs::read(source.join("campaign-result.json"))
            .map_err(|error| format!("cannot read campaign result: {error}"))?,
    )
    .map_err(|error| format!("cannot parse campaign result: {error}"))?;
    verify_recorded_campaign_guidance(campaign.guidance, campaign.coverage, &recorded)?;
    let checkpoint = starting_state::boot_or_load(
        &mut topology,
        &output.join("checkpoint"),
        &campaign.driver,
        verified_root,
    )?;
    let base = serde_json::to_vec(&topology)
        .map_err(|error| format!("cannot encode campaign base plan: {error}"))?;
    let mut checkpoints = CampaignCheckpointTree::new(checkpoint);
    let (property, mut schedule, mut locked_machine_execution_traces) =
        campaign_counterexample(&campaign, source, &recorded)?;
    let original_operations = schedule
        .operations
        .iter()
        .map(|operation| campaign_operation_choice_name(&campaign, *operation))
        .collect::<Vec<_>>();
    let original_faults = campaign_fault_names(&campaign, &schedule.faults);
    let attempts = output.join("minimization-attempts");
    fs::create_dir_all(&attempts).map_err(|error| error.to_string())?;
    let mut attempt = 0_usize;
    let faults = schedule.faults.clone();
    let original_operation_choices = schedule.operations.clone();
    let original_thread_schedule_prefixes = schedule.thread_schedule_prefixes.clone();
    let (operations, operation_attempts) =
        minimize_campaign_items(schedule.operations.clone(), 1, |operations| {
            let candidate = CampaignSchedule {
                operations: operations.to_vec(),
                faults: faults.clone(),
                thread_schedule_prefixes: campaign_prefixes_for_subsequence(
                    &original_operation_choices,
                    &original_thread_schedule_prefixes,
                    operations,
                ),
            };
            if !campaign_required_faults_apply(&campaign, &candidate) {
                return Ok(false);
            }
            let directory = attempts.join(format!("{attempt:03}"));
            attempt += 1;
            let reproduced = execute_campaign_minimization_attempt(
                &topology,
                &campaign,
                &base,
                &mut checkpoints,
                &candidate,
                &property,
                output,
                &directory,
            )?;
            if reproduced {
                locked_machine_execution_traces =
                    Some(campaign_machine_execution_traces(&directory)?);
            }
            Ok(reproduced)
        })?;
    schedule.operations = operations;
    schedule.thread_schedule_prefixes = campaign_prefixes_for_subsequence(
        &original_operation_choices,
        &original_thread_schedule_prefixes,
        &schedule.operations,
    );
    let operations = schedule.operations.clone();
    let selected_faults = schedule.faults.clone();
    let required_faults = selected_faults
        .iter()
        .copied()
        .filter(|index| campaign.faults[*index].required)
        .collect::<Vec<_>>();
    let optional_faults = selected_faults
        .iter()
        .copied()
        .filter(|index| !campaign.faults[*index].required)
        .collect::<Vec<_>>();
    let combine_faults = |optional: &[usize]| {
        selected_faults
            .iter()
            .copied()
            .filter(|index| required_faults.contains(index) || optional.contains(index))
            .collect::<Vec<_>>()
    };
    let (optional_faults, fault_attempts) =
        minimize_campaign_items(optional_faults, 0, |optional| {
            let candidate = CampaignSchedule {
                operations: operations.clone(),
                faults: combine_faults(optional),
                thread_schedule_prefixes: schedule.thread_schedule_prefixes.clone(),
            };
            let directory = attempts.join(format!("{attempt:03}"));
            attempt += 1;
            let reproduced = execute_campaign_minimization_attempt(
                &topology,
                &campaign,
                &base,
                &mut checkpoints,
                &candidate,
                &property,
                output,
                &directory,
            )?;
            if reproduced {
                locked_machine_execution_traces =
                    Some(campaign_machine_execution_traces(&directory)?);
            }
            Ok(reproduced)
        })?;
    schedule.faults = combine_faults(&optional_faults);
    // Rebuild the winning schedule from the retained ready root under the
    // machine control stream that demonstrated the failure. Reusing a prefix
    // from a rejected minimization candidate could silently select a sibling
    // guest interleaving and erase the counterexample.
    let mut final_checkpoints = CampaignCheckpointTree::new(checkpoints.root.clone());
    let prefix = match final_checkpoints.checkpoint_for_guarded_schedule(
        &topology,
        &campaign,
        &schedule,
        output,
        locked_machine_execution_traces.as_ref(),
    )? {
        CampaignPrefixResult::Ready(prefix) => prefix,
        CampaignPrefixResult::MarkerGuardRejected | CampaignPrefixResult::SerialGuardRejected => {
            return Err(
                "minimized campaign history no longer satisfies its operation guards".to_owned(),
            );
        }
    };
    let mut replay: TopologyPlan = serde_json::from_slice(&base)
        .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
    apply_campaign_schedule(&mut replay, &campaign, &schedule, &prefix.events)?;
    add_counterexample_check(&mut replay, &property)?;
    let mut final_plan: TopologyPlan = serde_json::from_slice(
        &serde_json::to_vec(&replay)
            .map_err(|error| format!("cannot encode campaign replay plan: {error}"))?,
    )
    .map_err(|error| format!("cannot decode campaign tail plan: {error}"))?;
    clear_campaign_events(&mut final_plan);
    let result = execute(
        final_plan,
        output,
        Some(&prefix.checkpoint),
        ExecutionCompletion::CampaignCheckpoint,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        locked_machine_execution_traces,
        None,
    );
    write_replay_plan(&output.join("replay-plan.json"), &replay)?;
    if output.join("topology-result.json").exists() {
        write_campaign_prefix_actions(output, prefix.actions)?;
    }
    result?;
    let minimized_operations = schedule
        .operations
        .iter()
        .map(|operation| campaign_operation_choice_name(&campaign, *operation))
        .collect::<Vec<_>>();
    fs::write(
        output.join("minimization.json"),
        serde_json::to_vec_pretty(&CampaignMinimization {
            property: property.name.clone(),
            original_operations,
            minimized_operations,
            original_thread_schedule_prefixes: schedule
                .operations
                .iter()
                .any(|choice| {
                    campaign.operations[choice.operation]
                        .thread_schedule_exploration
                        .is_some()
                })
                .then(|| original_thread_schedule_prefixes.clone())
                .unwrap_or_default(),
            minimized_thread_schedule_prefixes: schedule
                .operations
                .iter()
                .any(|choice| {
                    campaign.operations[choice.operation]
                        .thread_schedule_exploration
                        .is_some()
                })
                .then(|| schedule.thread_schedule_prefixes.clone())
                .unwrap_or_default(),
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
fn minimize_campaign_items<T, F>(
    mut items: Vec<T>,
    minimum_len: usize,
    mut property_fails: F,
) -> Result<(Vec<T>, usize), String>
where
    T: Clone,
    F: FnMut(&[T]) -> Result<bool, String>,
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
        .checkpoint_for_guarded_schedule(topology, campaign, schedule, output, None)?
    {
        CampaignPrefixResult::Ready(prefix) => prefix,
        CampaignPrefixResult::MarkerGuardRejected | CampaignPrefixResult::SerialGuardRejected => {
            return Ok(false);
        }
    };
    let mut replay: TopologyPlan = serde_json::from_slice(base)
        .map_err(|error| format!("cannot decode campaign base plan: {error}"))?;
    apply_campaign_schedule(&mut replay, campaign, schedule, &prefix.events)?;
    let mut plan: TopologyPlan = serde_json::from_slice(
        &serde_json::to_vec(&replay)
            .map_err(|error| format!("cannot encode campaign replay plan: {error}"))?,
    )
    .map_err(|error| format!("cannot decode campaign tail plan: {error}"))?;
    clear_campaign_events(&mut plan);
    let result = execute(
        plan,
        directory,
        Some(&prefix.checkpoint),
        ExecutionCompletion::CampaignCheckpoint,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    write_replay_plan(&directory.join("replay-plan.json"), &replay)?;
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
        (
            PropertyKind::Always
            | PropertyKind::AlwaysOrUnreachable
            | PropertyKind::Sometimes
            | PropertyKind::Reachable,
            false,
        ) => CheckKind::SerialNotContains,
        (PropertyKind::Unreachable, true) => CheckKind::SerialPropertyMatches,
        (
            PropertyKind::Always
            | PropertyKind::AlwaysOrUnreachable
            | PropertyKind::Sometimes
            | PropertyKind::Reachable,
            true,
        ) => CheckKind::SerialPropertyDoesNotMatch,
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

/// Corpus verdict for one property: `true` when the recorded runs falsify it.
fn property_corpus_failed(kind: PropertyKind, matches: &[bool]) -> bool {
    match kind {
        // Every timeline must report the property.
        PropertyKind::Always => matches.iter().any(|matched| !matched),
        // Every timeline must report it, or none may reach it at all; a
        // corpus where only some timelines report it is inconsistent.
        PropertyKind::AlwaysOrUnreachable => {
            matches.iter().any(|matched| !matched) && matches.iter().any(|matched| *matched)
        }
        // No timeline may report the property.
        PropertyKind::Unreachable => matches.iter().any(|matched| *matched),
        // At least one timeline must report the property.
        PropertyKind::Sometimes | PropertyKind::Reachable => {
            matches.iter().all(|matched| !matched)
        }
    }
}

/// First recorded run that falsifies the property, for the counterexample.
fn property_first_failing_run(kind: PropertyKind, matches: &[bool]) -> Option<usize> {
    match kind {
        PropertyKind::Always | PropertyKind::AlwaysOrUnreachable => {
            matches.iter().position(|matched| !matched)
        }
        PropertyKind::Unreachable => matches.iter().position(|matched| *matched),
        PropertyKind::Sometimes | PropertyKind::Reachable => Some(0),
    }
}

fn campaign_counterexample(
    campaign: &CampaignPlan,
    source: &Path,
    recorded: &RecordedCampaignResult,
) -> Result<
    (
        CampaignProperty,
        CampaignSchedule,
        Option<BTreeMap<String, Vec<String>>>,
    ),
    String,
> {
    for property in &campaign.properties {
        let matches = recorded
            .runs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                property_matches_in_run(property, &source.join("runs").join(format!("{index:03}")))
            })
            .collect::<Vec<_>>();
        let property_failed = property_corpus_failed(property.kind, &matches);
        if !property_failed {
            continue;
        }
        let index = property_first_failing_run(property.kind, &matches)
            .expect("failed property has a recorded run");
        let recorded_run = &recorded.runs[index];
        let operations = recorded_run
            .operations
            .iter()
            .map(|name| campaign_operation_choice_by_name(campaign, name))
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
            CampaignSchedule {
                thread_schedule_prefixes: if recorded_run.thread_schedule_prefixes.is_empty() {
                    vec![Vec::new(); operations.len()]
                } else {
                    recorded_run.thread_schedule_prefixes.clone()
                },
                operations,
                faults,
            },
            (!recorded_run.machine_execution_traces.is_empty())
                .then(|| recorded_run.machine_execution_traces.clone()),
        ));
    }
    Err("campaign bundle has no failing property to minimize".to_owned())
}

fn property_fails_in_run(property: &CampaignProperty, run: &Path) -> bool {
    let matched = property_matches_in_run(property, run);
    match property.kind {
        PropertyKind::Always | PropertyKind::AlwaysOrUnreachable => !matched,
        PropertyKind::Unreachable => matched,
        PropertyKind::Sometimes | PropertyKind::Reachable => !matched,
    }
}

fn campaign_prefixes_for_subsequence(
    original: &[CampaignOperationChoice],
    prefixes: &[Vec<u8>],
    selected: &[CampaignOperationChoice],
) -> Vec<Vec<u8>> {
    let mut cursor = 0;
    selected
        .iter()
        .map(|choice| {
            let relative = original[cursor..]
                .iter()
                .position(|candidate| candidate == choice)
                .expect("minimized operations remain an ordered subsequence");
            cursor += relative;
            let prefix = prefixes.get(cursor).cloned().unwrap_or_default();
            cursor += 1;
            prefix
        })
        .collect()
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

/// Return declared properties for which this one timeline produced useful
/// search evidence. A match witnesses `sometimes` and `reachable`; a miss
/// witnesses an `always` counterexample; and a match witnesses an
/// `unreachable` counterexample. Final property status still aggregates the
/// complete retained corpus.
fn campaign_property_witnesses(campaign: &CampaignPlan, run: &Path) -> Vec<String> {
    campaign
        .properties
        .iter()
        .filter_map(|property| {
            let matched = property_matches_in_run(property, run);
            let witness = match property.kind {
                PropertyKind::Always | PropertyKind::AlwaysOrUnreachable => !matched,
                PropertyKind::Sometimes | PropertyKind::Reachable | PropertyKind::Unreachable => {
                    matched
                }
            };
            witness.then(|| property.name.clone())
        })
        .collect()
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct CampaignSchedule {
    operations: Vec<CampaignOperationChoice>,
    faults: Vec<usize>,
    /// One runnable-choice prefix per operation occurrence. Non-exploring
    /// operations retain an empty entry, keeping the identity position-stable.
    thread_schedule_prefixes: Vec<Vec<u8>>,
}

/// An operation remains the stable target for guards, stages, use bounds, and
/// faults. Its input case is the variable that expands the campaign corpus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct CampaignOperationChoice {
    operation: usize,
    input: usize,
}

fn campaign_schedule_decision_prefix(schedule: &CampaignSchedule) -> Vec<String> {
    let mut prefix = Vec::new();
    for (position, choice) in schedule.operations.iter().enumerate() {
        prefix.push(format!("operation:{}:{}", choice.operation, choice.input));
        if let Some(choices) = schedule.thread_schedule_prefixes.get(position) {
            prefix.extend(
                choices
                    .iter()
                    .enumerate()
                    .map(|(index, selected)| format!("thread:{index}:{selected}")),
            );
        }
    }
    prefix.extend(schedule.faults.iter().map(|fault| format!("fault:{fault}")));
    prefix
}

fn campaign_decision_trace(
    campaign: &CampaignPlan,
    schedule: &CampaignSchedule,
    timeline: &[CampaignTimelineBoundary],
) -> Vec<String> {
    let mut trace = Vec::new();
    if let Some(template) = campaign_schedule_test_template(campaign, schedule) {
        trace.push(format!("test_template:{template}"));
    }
    for (position, (choice, boundary)) in schedule.operations.iter().zip(timeline).enumerate() {
        trace.push(format!(
            "boundary:{position}:operation:{}",
            campaign_operation_choice_name(campaign, *choice)
        ));
        if let Some(command) = campaign.operations[choice.operation].command {
            trace.push(format!(
                "boundary:{position}:command:{}",
                campaign_test_command_name(command)
            ));
        }
        if let Some(path) = &campaign.operations[choice.operation].test_command_path {
            trace.push(format!("boundary:{position}:test_command:{path}"));
        }
        trace.extend(
            boundary
                .terminated_command_services
                .iter()
                .map(|service| format!("boundary:{position}:terminate_commands:{service}")),
        );
        trace.push(format!(
            "boundary:{position}:input:{}:{}",
            boundary.service, boundary.input.sha256
        ));
        for (service, choices) in &boundary.new_structured_choices {
            trace.extend(choices.iter().map(|choice| {
                format!(
                    "boundary:{position}:choice:{service}:{}:{}/{}",
                    choice.name, choice.selected, choice.upper_exclusive
                )
            }));
        }
        for (service, decisions) in &boundary.new_thread_scheduling_decisions {
            trace.extend(decisions.iter().map(|decision| {
                format!(
                    "boundary:{position}:thread:{service}:{}:{}:{}",
                    decision.decision, decision.runnable_mask, decision.selected_thread
                )
            }));
        }
        trace.extend(boundary.actions.iter().map(|action| {
            format!(
                "boundary:{position}:action:{}:{}:{}",
                action.kind, action.target, action.detail
            )
        }));
    }
    trace
}

fn campaign_test_command_name(command: CampaignTestCommand) -> &'static str {
    match command {
        CampaignTestCommand::First => "first",
        CampaignTestCommand::ParallelDriver => "parallel_driver",
        CampaignTestCommand::SerialDriver => "serial_driver",
        CampaignTestCommand::SingletonDriver => "singleton_driver",
        CampaignTestCommand::Anytime => "anytime",
        CampaignTestCommand::Eventually => "eventually",
        CampaignTestCommand::Finally => "finally",
    }
}

fn common_decision_prefix(left: &[String], right: &[String]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

/// Older locked plans have no operation service and retain the designated
/// campaign driver as their target. New normalized plans always name it.
fn campaign_operation_service<'a>(
    campaign: &'a CampaignPlan,
    choice: CampaignOperationChoice,
) -> &'a str {
    let service = &campaign.operations[choice.operation].service;
    if service.is_empty() {
        &campaign.driver
    } else {
        service
    }
}

fn campaign_operation_inputs(operation: &CampaignOperation) -> Vec<CampaignOperationInput> {
    if operation.inputs.is_empty() {
        return operation
            .input_hex
            .as_ref()
            .map(|input_hex| CampaignOperationInput {
                name: "default".to_owned(),
                input_hex: input_hex.clone(),
                choices: BTreeMap::new(),
                thread_schedule: Vec::new(),
                input_template: None,
                input_captures: BTreeMap::new(),
                requires: Vec::new(),
                excludes: Vec::new(),
                max_uses: None,
                requires_state: BTreeMap::new(),
                sets_state: BTreeMap::new(),
            })
            .into_iter()
            .collect();
    }
    operation.inputs.clone()
}

fn campaign_operation_choices(
    campaign: &CampaignPlan,
    test_template: Option<&str>,
) -> Vec<CampaignOperationChoice> {
    campaign
        .operations
        .iter()
        .enumerate()
        .filter(|(_, operation)| {
            test_template
                .is_none_or(|template| operation.test_template.as_deref() == Some(template))
        })
        .flat_map(|(operation, definition)| {
            (0..campaign_operation_inputs(definition).len())
                .map(move |input| CampaignOperationChoice { operation, input })
        })
        .collect()
}

fn campaign_schedule_test_template<'a>(
    campaign: &'a CampaignPlan,
    schedule: &CampaignSchedule,
) -> Option<&'a str> {
    schedule
        .operations
        .first()
        .and_then(|choice| {
            campaign.operations[choice.operation]
                .test_template
                .as_deref()
        })
        .or(campaign.test_template.as_deref())
}

fn campaign_operation_input(
    campaign: &CampaignPlan,
    choice: CampaignOperationChoice,
) -> Result<CampaignOperationInput, String> {
    let operation = campaign.operations.get(choice.operation).ok_or_else(|| {
        format!(
            "campaign operation index {} is not declared",
            choice.operation
        )
    })?;
    campaign_operation_inputs(operation)
        .get(choice.input)
        .cloned()
        .ok_or_else(|| {
            format!(
                "campaign operation {:?} input index {} is not declared",
                operation.name, choice.input
            )
        })
}

fn verify_campaign_structured_choices(
    campaign: &CampaignPlan,
    schedule: &CampaignSchedule,
    timeline: &[CampaignTimelineBoundary],
) -> Result<(), String> {
    for (choice, boundary) in schedule.operations.iter().zip(timeline) {
        let operation = &campaign.operations[choice.operation];
        let input = campaign_operation_input(campaign, *choice)?;
        let mut observed = BTreeMap::new();
        for (service, decisions) in &boundary.new_structured_choices {
            if service != &boundary.service {
                return Err(format!(
                    "campaign operation {:?} observed a structured choice from unexpected service {service:?}",
                    operation.name
                ));
            }
            for decision in decisions {
                if observed
                    .insert(
                        decision.name.clone(),
                        (decision.upper_exclusive, decision.selected),
                    )
                    .is_some()
                {
                    return Err(format!(
                        "campaign operation {:?} used structured choice {:?} more than once",
                        operation.name, decision.name
                    ));
                }
                let expected_bound =
                    operation.choice_bounds.get(&decision.name).ok_or_else(|| {
                        format!(
                            "campaign operation {:?} used undeclared structured choice {:?}",
                            operation.name, decision.name
                        )
                    })?;
                let expected_selected = input.choices.get(&decision.name).ok_or_else(|| {
                    format!(
                        "campaign operation {:?} input {:?} has no assignment for structured choice {:?}",
                        operation.name, input.name, decision.name
                    )
                })?;
                if decision.upper_exclusive != *expected_bound
                    || decision.selected != *expected_selected
                {
                    return Err(format!(
                        "campaign operation {:?} structured choice {:?} diverged: expected bound {} value {}, observed bound {} value {}",
                        operation.name,
                        decision.name,
                        expected_bound,
                        expected_selected,
                        decision.upper_exclusive,
                        decision.selected,
                    ));
                }
            }
        }
        for (name, expected_selected) in &input.choices {
            let expected_bound = operation.choice_bounds.get(name).ok_or_else(|| {
                format!(
                    "campaign operation {:?} input {:?} assigned undeclared structured choice {name:?}",
                    operation.name, input.name
                )
            })?;
            if !observed.contains_key(name) {
                return Err(format!(
                    "campaign operation {:?} did not record assigned structured choice {name:?} at its point of use (expected bound {} value {})",
                    operation.name, expected_bound, expected_selected
                ));
            }
        }
    }
    Ok(())
}

fn campaign_input_reference_matches(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
    reference: &CampaignOperationInputReference,
) -> bool {
    history.iter().any(|choice| {
        let operation = &campaign.operations[choice.operation];
        operation.name == reference.operation
            && reference.input.as_ref().is_none_or(|expected| {
                campaign_operation_input(campaign, *choice)
                    .map(|input| input.name == *expected)
                    .unwrap_or(false)
            })
    })
}

fn campaign_input_reference_name(reference: &CampaignOperationInputReference) -> String {
    reference
        .input
        .as_ref()
        .map(|input| format!("{}[{input}]", reference.operation))
        .unwrap_or_else(|| reference.operation.clone())
}

fn campaign_fault_matches_operation(
    campaign: &CampaignPlan,
    fault: &CampaignFault,
    choice: CampaignOperationChoice,
) -> bool {
    if let Some(reference) = &fault.after_input {
        return campaign_input_reference_matches(campaign, &[choice], reference);
    }
    fault.after.as_deref() == Some(&campaign.operations[choice.operation].name)
}

fn campaign_fault_barrier_name(fault: &CampaignFault) -> String {
    fault
        .after_input
        .as_ref()
        .map(campaign_input_reference_name)
        .or_else(|| fault.after.clone())
        .expect("validated campaign topology action has a barrier")
}

fn campaign_operation_choice_name(
    campaign: &CampaignPlan,
    choice: CampaignOperationChoice,
) -> String {
    let operation = &campaign.operations[choice.operation];
    let inputs = campaign_operation_inputs(operation);
    let input = &inputs[choice.input];
    if inputs.len() == 1 && input.name == "default" {
        operation.name.clone()
    } else {
        format!("{}[{}]", operation.name, input.name)
    }
}

fn campaign_operation_choice_by_name(
    campaign: &CampaignPlan,
    name: &str,
) -> Result<CampaignOperationChoice, String> {
    let matches = campaign_operation_choices(campaign, None)
        .into_iter()
        .filter(|choice| campaign_operation_choice_name(campaign, *choice) == name)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [choice] => Ok(*choice),
        [] => Err(format!(
            "recorded campaign operation is not declared: {name}"
        )),
        _ => Err(format!("recorded campaign operation is ambiguous: {name}")),
    }
}

fn campaign_state_after(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
) -> BTreeMap<String, String> {
    let mut state = campaign.state.clone();
    for choice in history {
        let operation = &campaign.operations[choice.operation];
        state.extend(operation.sets_state.clone());
        let input = campaign_operation_input(campaign, *choice)
            .expect("recorded campaign operation choice has a declared input");
        // A case specializes its logical operation, so its explicit value wins
        // when both transition layers set the same state key.
        state.extend(input.sets_state);
    }
    state
}

fn campaign_state_matches(
    state: &BTreeMap<String, String>,
    required: &BTreeMap<String, String>,
) -> bool {
    required
        .iter()
        .all(|(key, value)| state.get(key) == Some(value))
}

fn campaign_operation_marker_guards_are_ready(
    campaign: &CampaignPlan,
    checkpoint: &CampaignCheckpoint,
    choice: CampaignOperationChoice,
) -> bool {
    let markers = campaign_checkpoint_markers(checkpoint);
    let candidate = &campaign.operations[choice.operation];
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
    choice: CampaignOperationChoice,
) -> bool {
    let candidate = &campaign.operations[choice.operation];
    let service = campaign_operation_service(campaign, choice);
    candidate
        .requires_serial
        .as_ref()
        .map(|guard| campaign_serial_guard_matches(checkpoint, service, guard))
        .unwrap_or(true)
        && candidate
            .requires_serial_all
            .iter()
            .all(|guard| campaign_serial_guard_matches(checkpoint, service, guard))
        && candidate
            .excludes_serial
            .as_ref()
            .map(|guard| !campaign_serial_guard_matches(checkpoint, service, guard))
            .unwrap_or(true)
        && candidate
            .excludes_serial_any
            .iter()
            .all(|guard| !campaign_serial_guard_matches(checkpoint, service, guard))
        && candidate
            .requires_serial_joins
            .iter()
            .all(|join| campaign_serial_join_matches(checkpoint, service, join))
        && candidate
            .excludes_serial_joins
            .iter()
            .all(|join| !campaign_serial_join_matches(checkpoint, service, join))
        && candidate
            .requires_serial_evidence
            .as_ref()
            .map(|evidence| campaign_serial_evidence_matches(checkpoint, service, evidence))
            .unwrap_or(true)
        && candidate
            .excludes_serial_evidence
            .as_ref()
            .map(|evidence| !campaign_serial_evidence_matches(checkpoint, service, evidence))
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

fn serial_path_terminal_events(
    serial: &[u8],
    pointers: &[String],
    steps: &[JsonPredicate],
) -> Vec<(serde_json::Value, usize, serde_json::Value)> {
    let step_events = steps
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
                    json_path_key(&event, pointers).map(|key| (position, key, event))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let Some((first, later)) = step_events.split_first() else {
        return Vec::new();
    };
    first
        .iter()
        .flat_map(|(position, key, event)| {
            if later.is_empty() {
                vec![(key.clone(), *position, event.clone())]
            } else {
                serial_path_terminal_events_from(later, *position, key)
                    .into_iter()
                    .map(|(position, event)| (key.clone(), position, event))
                    .collect()
            }
        })
        .collect()
}

fn serial_path_terminal_events_from(
    steps: &[Vec<(usize, serde_json::Value, serde_json::Value)>],
    previous_position: usize,
    key: &serde_json::Value,
) -> Vec<(usize, serde_json::Value)> {
    let Some((step, later)) = steps.split_first() else {
        return Vec::new();
    };
    step.iter()
        .filter(|(position, value, _)| *position > previous_position && value == key)
        .flat_map(|(position, _, event)| {
            if later.is_empty() {
                vec![(*position, event.clone())]
            } else {
                serial_path_terminal_events_from(later, *position, key)
            }
        })
        .collect()
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

fn serial_workflow_stage_values(
    serial: &[u8],
    workflow: &SerialWorkflow,
    stage: &SerialWorkflowStage,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    serial_path_values(
        serial,
        &SerialPath {
            service: None,
            pointers: if stage.pointers.is_empty() {
                workflow.pointers.clone()
            } else {
                stage.pointers.clone()
            },
            steps: stage.steps.clone(),
            quantifier: SerialJoinQuantifier::Any,
            occurs: None,
        },
    )
}

fn serial_workflow_matches(
    workflow: &SerialWorkflow,
    stage_values: Vec<(Vec<serde_json::Value>, Vec<serde_json::Value>)>,
) -> bool {
    let Some((candidates, _)) = stage_values.first() else {
        return false;
    };
    let matches = candidates
        .iter()
        .filter(|key| {
            stage_values
                .iter()
                .all(|(_, matched)| matched.iter().any(|value| value == *key))
        })
        .cloned()
        .collect();
    serial_match_requirements_met(
        candidates.clone(),
        matches,
        workflow.quantifier,
        workflow.occurs.as_ref(),
    )
}

fn campaign_serial_workflow_matches(
    checkpoint: &CampaignCheckpoint,
    workflow: &SerialWorkflow,
) -> bool {
    serial_workflow_matches(
        workflow,
        workflow
            .stages
            .iter()
            .map(|stage| {
                serial_workflow_stage_values(
                    &campaign_checkpoint_serial(checkpoint, &stage.service),
                    workflow,
                    stage,
                )
            })
            .collect(),
    )
}

fn campaign_workflow_capture_values(
    checkpoint: &CampaignCheckpoint,
    workflow: &SerialWorkflow,
    pointer: &str,
) -> Vec<serde_json::Value> {
    let stage_values = workflow
        .stages
        .iter()
        .map(|stage| {
            serial_workflow_stage_values(
                &campaign_checkpoint_serial(checkpoint, &stage.service),
                workflow,
                stage,
            )
        })
        .collect::<Vec<_>>();
    let Some((candidates, _)) = stage_values.first() else {
        return Vec::new();
    };
    let matches = candidates
        .iter()
        .filter(|key| {
            stage_values
                .iter()
                .all(|(_, matched)| matched.iter().any(|value| value == *key))
        })
        .cloned()
        .collect::<Vec<_>>();
    if !serial_match_requirements_met(
        candidates.clone(),
        matches.clone(),
        workflow.quantifier,
        workflow.occurs.as_ref(),
    ) {
        return Vec::new();
    }
    let Some(stage) = workflow.stages.last() else {
        return Vec::new();
    };
    let stage_pointers = if stage.pointers.is_empty() {
        &workflow.pointers
    } else {
        &stage.pointers
    };
    serial_path_terminal_events(
        &campaign_checkpoint_serial(checkpoint, &stage.service),
        stage_pointers,
        &stage.steps,
    )
    .into_iter()
    .filter(|(key, _, _)| matches.iter().any(|candidate| candidate == key))
    .filter_map(|(_, _, event)| event.pointer(pointer).cloned())
    .collect()
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
    } else if let Some(workflow) = &evidence.workflow {
        campaign_serial_workflow_matches(checkpoint, workflow)
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

#[derive(Clone, Debug, Serialize)]
struct CampaignGuidanceObservation {
    operations: Vec<CampaignOperationChoice>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    decision_prefix: Vec<String>,
    novel_markers: usize,
    novel_instructions: usize,
    novel_checkpoint_pcs: usize,
    novel_application_blocks: usize,
    #[serde(skip_serializing_if = "is_zero")]
    novel_structured_choices: usize,
    #[serde(skip_serializing_if = "is_zero")]
    novel_scheduling_decisions: usize,
    novel_state: bool,
    failed: bool,
    property_witnesses: Vec<String>,
}

impl CampaignGuidanceLedger {
    fn from_observations(observations: &[CampaignGuidanceObservation]) -> Self {
        let encoded =
            serde_json::to_vec(observations).expect("campaign guidance observations serialize");
        Self {
            observations: observations.len(),
            sha256: format!("{:x}", Sha256::digest(encoded)),
        }
    }
}

impl CampaignSearchEvidence {
    fn from_search(
        checkpoints: &CampaignCheckpointTree,
        leaf_restores: usize,
        leaf_cow_restore_bytes: u64,
        observations: &[CampaignGuidanceObservation],
    ) -> Self {
        let ledger = CampaignGuidanceLedger::from_observations(observations);
        Self {
            checkpoint: checkpoints.economics(leaf_restores, leaf_cow_restore_bytes),
            guidance_observations: ledger.observations,
            guidance_sha256: ledger.sha256,
        }
    }
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
        let applicable = campaign
            .faults
            .iter()
            .enumerate()
            .filter_map(|(index, fault)| {
                campaign_fault_applies(fault, &history, campaign).then_some(index)
            })
            .collect::<Vec<_>>();
        for faults in campaign_fault_selections(
            &campaign.faults,
            &applicable,
            usize::from(campaign.max_faults_per_run),
        ) {
            schedules.push(CampaignSchedule {
                operations: history.clone(),
                faults,
                thread_schedule_prefixes: vec![Vec::new(); history.len()],
            });
            if schedules.len() == MAX_CAMPAIGN_CANDIDATES {
                return schedules;
            }
        }
    }
    schedules
}

/// Expand only branches that the runtime proved runnable. For each observed
/// choice point, retain the path that reached it and fork once for every other
/// runnable thread. Repeating this after each run builds a bounded execution
/// tree without inventing impossible static schedules.
fn extend_runnable_prefix_schedules(
    campaign: &CampaignPlan,
    source: &CampaignSchedule,
    timeline: &[CampaignTimelineBoundary],
    schedules: &mut Vec<CampaignSchedule>,
    pending: &mut Vec<usize>,
) {
    for (operation_index, (choice, boundary)) in source.operations.iter().zip(timeline).enumerate()
    {
        let definition = &campaign.operations[choice.operation];
        let Some(exploration) = &definition.thread_schedule_exploration else {
            continue;
        };
        if exploration.strategy != "runnable_prefixes" {
            continue;
        }
        let service = campaign_operation_service(campaign, *choice);
        let decisions = boundary
            .new_thread_scheduling_decisions
            .get(service)
            .into_iter()
            .flatten()
            .filter_map(|decision| {
                let mask = decision.runnable_mask.strip_prefix("0x")?;
                let mask = u32::from_str_radix(mask, 16).ok()?;
                (mask.count_ones() > 1).then_some((mask, decision.selected_thread))
            })
            .take(usize::from(exploration.max_choices))
            .collect::<Vec<_>>();
        let actual = decisions
            .iter()
            .map(|(_, selected)| *selected)
            .collect::<Vec<_>>();
        for (depth, (mask, selected)) in decisions.iter().copied().enumerate() {
            for alternative in 0_u8..32 {
                if alternative == selected || mask & (1_u32 << alternative) == 0 {
                    continue;
                }
                let variants = schedules
                    .iter()
                    .filter(|candidate| {
                        candidate.operations == source.operations
                            && candidate.faults == source.faults
                            && candidate
                                .thread_schedule_prefixes
                                .get(operation_index)
                                .is_some_and(|prefix| !prefix.is_empty())
                    })
                    .count();
                if variants.saturating_add(1) >= usize::from(exploration.max_variants)
                    || schedules.len() == MAX_CAMPAIGN_CANDIDATES
                {
                    return;
                }
                let mut candidate = source.clone();
                candidate.thread_schedule_prefixes[operation_index] = actual[..depth]
                    .iter()
                    .copied()
                    .chain(std::iter::once(alternative))
                    .collect();
                for prefix in &mut candidate.thread_schedule_prefixes[operation_index + 1..] {
                    prefix.clear();
                }
                if schedules.iter().any(|existing| existing == &candidate) {
                    continue;
                }
                schedules.push(candidate);
                pending.push(schedules.len() - 1);
            }
        }
    }
}

/// Return every bounded selection while retaining faults marked as required.
/// With no applicable required fault, the empty selection stays first so
/// existing campaign plans keep their historical search order.
fn campaign_fault_selections(
    faults: &[CampaignFault],
    applicable: &[usize],
    maximum: usize,
) -> Vec<Vec<usize>> {
    let required = applicable
        .iter()
        .copied()
        .filter(|index| faults[*index].required)
        .collect::<Vec<_>>();
    if required.len() > maximum
        || required.iter().enumerate().any(|(offset, first)| {
            required[offset + 1..]
                .iter()
                .any(|second| !campaign_faults_compatible(&faults[*first], &faults[*second]))
        })
    {
        return Vec::new();
    }
    let optional = applicable
        .iter()
        .copied()
        .filter(|index| {
            !faults[*index].required
                && required
                    .iter()
                    .all(|required| campaign_faults_compatible(&faults[*required], &faults[*index]))
        })
        .collect::<Vec<_>>();
    let mut optional_selections = vec![Vec::new()];
    optional_selections.extend(campaign_fault_combinations(
        faults,
        &optional,
        maximum - required.len(),
    ));
    optional_selections
        .into_iter()
        .map(|selected| {
            applicable
                .iter()
                .copied()
                .filter(|index| faults[*index].required || selected.contains(index))
                .collect()
        })
        .collect()
}

/// Enumerate every ordered operation history, including repetitions, in stable
/// breadth-first order. A manifest controls the depth explicitly; the global
/// candidate cap remains the final guard for wide workloads and fault products.
fn campaign_operation_histories(campaign: &CampaignPlan) -> Vec<Vec<CampaignOperationChoice>> {
    let scopes = if campaign.test_templates.is_empty() {
        vec![None]
    } else {
        campaign
            .test_templates
            .iter()
            .map(|template| Some(template.as_str()))
            .collect::<Vec<_>>()
    };
    let mut by_scope = scopes
        .into_iter()
        .map(|scope| campaign_operation_histories_for_template(campaign, scope))
        .collect::<Vec<_>>();
    if by_scope.len() == 1 {
        return by_scope.pop().unwrap_or_default();
    }
    let mut histories = Vec::new();
    for index in 0..by_scope.iter().map(Vec::len).max().unwrap_or_default() {
        for scoped in &by_scope {
            if let Some(history) = scoped.get(index) {
                histories.push(history.clone());
                if histories.len() == MAX_CAMPAIGN_CANDIDATES {
                    return histories;
                }
            }
        }
    }
    histories
}

fn campaign_operation_histories_for_template(
    campaign: &CampaignPlan,
    test_template: Option<&str>,
) -> Vec<Vec<CampaignOperationChoice>> {
    let choices = campaign_operation_choices(campaign, test_template);
    let histories = ordered_operation_histories(
        choices.len(),
        usize::from(campaign.max_operations_per_run),
        |history, choice| {
            let resolved = history
                .iter()
                .map(|choice| choices[*choice])
                .collect::<Vec<_>>();
            campaign_operation_is_ready(campaign, &resolved, choices[choice])
        },
    )
    .into_iter()
    .map(|history| {
        history
            .into_iter()
            .map(|choice| choices[choice])
            .collect::<Vec<_>>()
    });
    if campaign_uses_test_commands(campaign) {
        histories
            .filter(|history| campaign_test_history_is_complete(campaign, history))
            .collect()
    } else {
        histories.collect()
    }
}

fn campaign_operation_is_ready(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
    choice: CampaignOperationChoice,
) -> bool {
    let candidate = &campaign.operations[choice.operation];
    let input = campaign_operation_input(campaign, choice)
        .expect("generated campaign operation choice has a declared input");
    let state = campaign_state_after(campaign, history);
    let candidate_stage = candidate.stage.as_ref().and_then(|stage| {
        campaign
            .stages
            .iter()
            .position(|declared| declared == stage)
    });
    let stages_are_ordered = history.iter().all(|prior| {
        let prior_stage = campaign.operations[prior.operation]
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
    let shell_process_ready = match (candidate.shell_phase, candidate.shell_process.as_deref()) {
        (Some(CampaignShellPhase::Launch), Some(process)) => {
            campaign_shell_process_balance(campaign, history, &candidate.service, process) == 0
        }
        (Some(CampaignShellPhase::Completion), Some(process)) => {
            campaign_shell_process_balance(campaign, history, &candidate.service, process) == 1
        }
        _ => true,
    };
    shell_process_ready
        && campaign_test_command_is_ready(campaign, history, choice)
        && stages_are_ordered
        && campaign_state_matches(&state, &candidate.requires_state)
        && campaign_state_matches(&state, &input.requires_state)
        && candidate.requires.iter().all(|requirement| {
            history
                .iter()
                .any(|prior| campaign.operations[prior.operation].name == requirement.as_str())
        })
        && candidate.excludes.iter().all(|exclusion| {
            history
                .iter()
                .all(|prior| campaign.operations[prior.operation].name != exclusion.as_str())
        })
        && candidate.max_uses.map_or(true, |maximum| {
            history
                .iter()
                .filter(|prior| prior.operation == choice.operation)
                .count()
                < usize::from(maximum)
        })
        && input
            .requires
            .iter()
            .all(|reference| campaign_input_reference_matches(campaign, history, reference))
        && input
            .excludes
            .iter()
            .all(|reference| !campaign_input_reference_matches(campaign, history, reference))
        && input.max_uses.map_or(true, |maximum| {
            history
                .iter()
                .filter(|prior| prior.operation == choice.operation && prior.input == choice.input)
                .count()
                < usize::from(maximum)
        })
}

fn campaign_uses_test_commands(campaign: &CampaignPlan) -> bool {
    campaign
        .operations
        .iter()
        .any(|operation| operation.command.is_some())
}

fn validate_campaign_test_commands(campaign: &CampaignPlan) -> Result<(), String> {
    if campaign.test_template.is_some() && !campaign.test_templates.is_empty() {
        return Err("campaign plan declares both test_template and test_templates".to_owned());
    }
    let declared_templates = campaign
        .test_templates
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if declared_templates.len() != campaign.test_templates.len()
        || declared_templates
            .iter()
            .any(|template| template.is_empty())
    {
        return Err("campaign plan has empty or duplicate test_templates".to_owned());
    }
    let declared = campaign
        .operations
        .iter()
        .filter(|operation| operation.command.is_some())
        .count();
    if declared == 0 {
        return Ok(());
    }
    if declared != campaign.operations.len() {
        return Err("test-command plan does not assign a role to every operation".to_owned());
    }
    if !campaign.stages.is_empty() {
        return Err("test-command plan also declares legacy stages".to_owned());
    }
    if !campaign.test_templates.is_empty()
        && campaign.operations.iter().any(|operation| {
            operation
                .test_template
                .as_deref()
                .is_none_or(|template| !declared_templates.contains(template))
        })
    {
        return Err(
            "multi-template campaign operation has no declared test-template scope".to_owned(),
        );
    }
    let scopes = if campaign.test_templates.is_empty() {
        vec![None]
    } else {
        campaign
            .test_templates
            .iter()
            .map(|template| Some(template.as_str()))
            .collect::<Vec<_>>()
    };
    for scope in scopes {
        if !campaign.operations.iter().any(|operation| {
            scope.is_none_or(|template| operation.test_template.as_deref() == Some(template))
                && matches!(
                    operation.command,
                    Some(
                        CampaignTestCommand::ParallelDriver
                            | CampaignTestCommand::SerialDriver
                            | CampaignTestCommand::SingletonDriver
                            | CampaignTestCommand::Anytime
                    )
                )
        }) {
            return Err(match scope {
                Some(template) => {
                    format!("test template {template:?} has no driver or anytime command")
                }
                None => "test-command plan has no driver or anytime command".to_owned(),
            });
        }
    }
    Ok(())
}

fn campaign_test_command_is_ready(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
    choice: CampaignOperationChoice,
) -> bool {
    if !campaign_uses_test_commands(campaign) {
        return true;
    }
    let Some(command) = campaign.operations[choice.operation].command else {
        return false;
    };
    let candidate_template = campaign.operations[choice.operation]
        .test_template
        .as_deref()
        .or(campaign.test_template.as_deref());
    if history.first().is_some_and(|prior| {
        campaign.operations[prior.operation]
            .test_template
            .as_deref()
            .or(campaign.test_template.as_deref())
            != candidate_template
    }) {
        return false;
    }
    let commands = history
        .iter()
        .filter_map(|prior| campaign.operations[prior.operation].command)
        .collect::<Vec<_>>();
    let first_declared = campaign.operations.iter().any(|operation| {
        operation
            .test_template
            .as_deref()
            .or(campaign.test_template.as_deref())
            == candidate_template
            && operation.command == Some(CampaignTestCommand::First)
    });
    let first_finished = commands.first() == Some(&CampaignTestCommand::First);
    let terminal_started = commands.iter().any(|prior| {
        matches!(
            prior,
            CampaignTestCommand::Eventually | CampaignTestCommand::Finally
        )
    });
    let singleton_started = commands.contains(&CampaignTestCommand::SingletonDriver);
    let ordinary_driver_started = commands.iter().any(|prior| {
        matches!(
            prior,
            CampaignTestCommand::ParallelDriver | CampaignTestCommand::SerialDriver
        )
    });
    let driver_started = singleton_started || ordinary_driver_started;
    let lifecycle_started = !first_declared || first_finished;
    match command {
        CampaignTestCommand::First => history.is_empty(),
        CampaignTestCommand::ParallelDriver => {
            lifecycle_started && !terminal_started && !singleton_started
        }
        CampaignTestCommand::SerialDriver => {
            lifecycle_started
                && !terminal_started
                && !singleton_started
                && !campaign_has_active_parallel_process(campaign, history)
        }
        CampaignTestCommand::SingletonDriver => {
            lifecycle_started
                && !terminal_started
                && !driver_started
                && !campaign_has_active_parallel_process(campaign, history)
        }
        CampaignTestCommand::Anytime => lifecycle_started && !terminal_started,
        CampaignTestCommand::Eventually => lifecycle_started && driver_started && !terminal_started,
        CampaignTestCommand::Finally => {
            lifecycle_started
                && driver_started
                && !terminal_started
                && !campaign_has_active_process(campaign, history)
        }
    }
}

fn campaign_test_history_is_complete(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
) -> bool {
    let commands = history
        .iter()
        .filter_map(|choice| campaign.operations[choice.operation].command)
        .collect::<Vec<_>>();
    let template = history.first().and_then(|choice| {
        campaign.operations[choice.operation]
            .test_template
            .as_deref()
            .or(campaign.test_template.as_deref())
    });
    let first_declared = campaign.operations.iter().any(|operation| {
        operation
            .test_template
            .as_deref()
            .or(campaign.test_template.as_deref())
            == template
            && operation.command == Some(CampaignTestCommand::First)
    });
    let driver_declared = campaign.operations.iter().any(|operation| {
        operation
            .test_template
            .as_deref()
            .or(campaign.test_template.as_deref())
            == template
            && matches!(
                operation.command,
                Some(
                    CampaignTestCommand::ParallelDriver
                        | CampaignTestCommand::SerialDriver
                        | CampaignTestCommand::SingletonDriver
                )
            )
    });
    let lifecycle_started =
        !first_declared || commands.first() == Some(&CampaignTestCommand::First);
    let useful = commands.iter().any(|command| {
        if driver_declared {
            matches!(
                command,
                CampaignTestCommand::ParallelDriver
                    | CampaignTestCommand::SerialDriver
                    | CampaignTestCommand::SingletonDriver
            )
        } else {
            *command == CampaignTestCommand::Anytime
        }
    });
    let eventual_terminates_live_commands =
        commands.last() == Some(&CampaignTestCommand::Eventually);
    lifecycle_started
        && useful
        && (eventual_terminates_live_commands || !campaign_has_active_process(campaign, history))
}

fn campaign_has_active_process(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
) -> bool {
    campaign.operations.iter().any(|operation| {
        operation.shell_process.as_deref().is_some_and(|process| {
            campaign_shell_process_balance(campaign, history, &operation.service, process) != 0
        })
    })
}

fn campaign_active_shell_process_services(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
) -> Vec<String> {
    campaign
        .operations
        .iter()
        .filter_map(|operation| {
            operation.shell_process.as_deref().and_then(|process| {
                (campaign_shell_process_balance(campaign, history, &operation.service, process)
                    != 0)
                    .then(|| operation.service.clone())
            })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn campaign_has_active_parallel_process(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
) -> bool {
    campaign.operations.iter().any(|operation| {
        operation.command == Some(CampaignTestCommand::ParallelDriver)
            && operation.shell_process.as_deref().is_some_and(|process| {
                campaign_shell_process_balance(campaign, history, &operation.service, process) != 0
            })
    })
}

/// Number of unmatched launches for one service-local process identity.
/// Compose validation prevents malformed phase declarations; this additional
/// scheduler rule removes impossible completion-first and double-launch
/// histories from the bounded corpus.
fn campaign_shell_process_balance(
    campaign: &CampaignPlan,
    history: &[CampaignOperationChoice],
    service: &str,
    process: &str,
) -> usize {
    history.iter().fold(0_usize, |balance, choice| {
        let prior = &campaign.operations[choice.operation];
        if prior.service != service || prior.shell_process.as_deref() != Some(process) {
            return balance;
        }
        match prior.shell_phase {
            Some(CampaignShellPhase::Launch) => balance.saturating_add(1),
            Some(CampaignShellPhase::Completion) => balance.saturating_sub(1),
            _ => balance,
        }
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
                .map(|name| campaign_operation_choice_by_name(campaign, name))
                .collect::<Result<Vec<_>, _>>()?;
            let selected_template = operations.first().and_then(|choice| {
                campaign.operations[choice.operation]
                    .test_template
                    .as_deref()
                    .or(campaign.test_template.as_deref())
            });
            if run
                .test_template
                .as_deref()
                .is_some_and(|recorded| selected_template != Some(recorded))
            {
                return Err(
                    "recorded campaign test template differs from its operations".to_owned(),
                );
            }
            if campaign_uses_test_commands(campaign)
                && !campaign_test_history_is_complete(campaign, &operations)
            {
                return Err("recorded test-command timeline violates its lifecycle".to_owned());
            }
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
            let thread_schedule_prefixes = if run.thread_schedule_prefixes.is_empty() {
                vec![Vec::new(); operations.len()]
            } else if run.thread_schedule_prefixes.len() == operations.len() {
                run.thread_schedule_prefixes.clone()
            } else {
                return Err(
                    "recorded thread schedule prefixes do not match its operations".to_owned(),
                );
            };
            Ok(CampaignSchedule {
                operations,
                faults,
                thread_schedule_prefixes,
            })
        })
        .collect()
}

/// Older bundles did not record a policy and remain replayable. New bundles
/// reject a plan whose declared policy differs from the policy that chose the
/// recorded corpus, rather than silently treating adaptive ordering as plain
/// coverage ordering.
fn verify_recorded_campaign_guidance(
    guidance: CampaignGuidance,
    coverage: CampaignCoverage,
    recorded: &RecordedCampaignResult,
) -> Result<(), String> {
    if recorded
        .guidance
        .is_some_and(|recorded_guidance| recorded_guidance != guidance)
    {
        return Err("recorded campaign guidance differs from replay plan".to_owned());
    }
    if recorded
        .coverage
        .is_some_and(|recorded_coverage| recorded_coverage != coverage)
    {
        return Err("recorded campaign coverage differs from replay plan".to_owned());
    }
    Ok(())
}

fn campaign_replay_mismatches(expected: &RecordedCampaignRun, actual: &CampaignRun) -> Vec<String> {
    let mut mismatches = Vec::new();
    if expected.test_template.is_some() && expected.test_template != actual.test_template {
        mismatches.push("test template".to_owned());
    }
    if expected.operations != actual.operations {
        mismatches.push("operations".to_owned());
    }
    if !expected.decision_trace.is_empty() && expected.decision_trace != actual.decision_trace {
        mismatches.push("decision trace".to_owned());
    }
    if !expected.thread_schedule_prefixes.is_empty()
        && expected.thread_schedule_prefixes != actual.thread_schedule_prefixes
    {
        mismatches.push("thread schedule prefixes".to_owned());
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
    if expected
        .guidance_ledger
        .as_ref()
        .is_some_and(|ledger| ledger != &actual.guidance_ledger)
    {
        mismatches.push("guidance ledger".to_owned());
    }
    if expected.guidance_evidence.is_some()
        && expected.guidance_evidence != actual.guidance_evidence
    {
        mismatches.push("posterior guidance evidence".to_owned());
    }
    if expected
        .property_witnesses
        .as_ref()
        .is_some_and(|witnesses| witnesses != &actual.property_witnesses)
    {
        mismatches.push("property guidance evidence".to_owned());
    }
    if !expected.timeline.is_empty()
        && !campaign_timeline_matches(&expected.timeline, &actual.timeline)
    {
        mismatches.push("operation-boundary timeline".to_owned());
    }
    if !expected.program_counters.is_empty() && expected.program_counters != actual.program_counters
    {
        mismatches.push("checkpoint program counters".to_owned());
    }
    if !expected.instruction_locations.is_empty()
        && expected.instruction_locations != actual.instruction_locations
    {
        mismatches.push("symbolized instruction locations".to_owned());
    }
    if !expected.novelty.is_empty() && expected.novelty != actual.novelty {
        mismatches.push("marker coverage".to_owned());
    }
    if !expected.instruction_novelty.is_empty()
        && expected.instruction_novelty != actual.instruction_novelty
    {
        mismatches.push("instruction-location coverage".to_owned());
    }
    if !expected.checkpoint_pc_novelty.is_empty()
        && expected.checkpoint_pc_novelty != actual.checkpoint_pc_novelty
    {
        mismatches.push("checkpoint-PC coverage".to_owned());
    }
    if !expected.application_blocks.is_empty()
        && expected.application_blocks != actual.application_blocks
    {
        mismatches.push("application coverage".to_owned());
    }
    if !expected.application_block_novelty.is_empty()
        && expected.application_block_novelty != actual.application_block_novelty
    {
        mismatches.push("application coverage novelty".to_owned());
    }
    if !expected.thread_scheduling.is_empty()
        && expected.thread_scheduling != actual.thread_scheduling
    {
        mismatches.push("thread-scheduling decisions".to_owned());
    }
    if !expected.thread_synchronization.is_empty()
        && expected.thread_synchronization != actual.thread_synchronization
    {
        mismatches.push("thread-synchronization events".to_owned());
    }
    if !expected.structured_choices.is_empty()
        && expected.structured_choices != actual.structured_choices
    {
        mismatches.push("structured-choice decisions".to_owned());
    }
    if !expected.execution_ledgers.is_empty()
        && expected.execution_ledgers != actual.execution_ledgers
    {
        mismatches.push("ordered KVM execution ledger".to_owned());
    }
    if !expected.machine_execution_ledgers.is_empty()
        && expected.machine_execution_ledgers != actual.machine_execution_ledgers
    {
        mismatches.push("machine-wide execution stream".to_owned());
    }
    if !expected.machine_execution_traces.is_empty()
        && expected.machine_execution_traces != actual.machine_execution_traces
    {
        mismatches.push("actively enforced machine execution trace".to_owned());
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

/// Portable checkpoint replay governs declared host inputs and application-
/// level decisions, not the exact Linux instruction at which a vCPU happened
/// to pause. Keep the complete low-level observations in each new result, but
/// do not turn them into claims the host-input contract does not make.
fn campaign_host_input_replay_mismatches(
    expected: &RecordedCampaignRun,
    actual: &CampaignRun,
) -> Vec<String> {
    let mut mismatches = campaign_replay_mismatches(expected, actual);
    mismatches.retain(|mismatch| {
        !matches!(
            mismatch.as_str(),
            "guidance ledger"
                | "posterior guidance evidence"
                | "operation-boundary timeline"
                | "checkpoint program counters"
                | "symbolized instruction locations"
                | "instruction-location coverage"
                | "checkpoint-PC coverage"
                | "ordered KVM execution ledger"
                | "machine-wide execution stream"
                | "actively enforced machine execution trace"
                | "topology-state coverage"
        )
    });
    if !expected.timeline.is_empty()
        && !campaign_host_input_timeline_matches(&expected.timeline, &actual.timeline)
    {
        mismatches.push("operation control timeline".to_owned());
    }
    if !expected.machine_execution_traces.is_empty()
        && !campaign_host_input_traces_match(
            &expected.machine_execution_traces,
            &actual.machine_execution_traces,
        )
    {
        mismatches.push("actively enforced host-input trace".to_owned());
    }
    mismatches
}

fn campaign_host_input_timeline_matches(
    expected: &[CampaignTimelineBoundary],
    actual: &[CampaignTimelineBoundary],
) -> bool {
    expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(expected, actual)| {
            (expected.id.is_empty() || expected.id == actual.id)
                && expected.operation == actual.operation
                && expected.command == actual.command
                && expected.test_command_path == actual.test_command_path
                && expected.terminated_command_services == actual.terminated_command_services
                && (expected.service.is_empty() || expected.service == actual.service)
                && (campaign_input_is_absent(&expected.input) || expected.input == actual.input)
                && (campaign_uart_delivery_is_absent(&expected.delivery)
                    || (expected.delivery.recorded == actual.delivery.recorded
                        && expected.delivery.accepted_bytes == actual.delivery.accepted_bytes
                        && expected.delivery.checkpoint == actual.delivery.checkpoint))
                && (campaign_uart_barrier_is_absent(&expected.barrier)
                    || (expected.barrier.recorded == actual.barrier.recorded
                        && expected.barrier.checkpoint == actual.barrier.checkpoint))
                && expected.actions == actual.actions
        })
}

fn campaign_host_input_traces_match(
    expected: &BTreeMap<String, Vec<String>>,
    actual: &BTreeMap<String, Vec<String>>,
) -> bool {
    expected.len() == actual.len()
        && expected.iter().all(|(name, expected)| {
            actual.get(name).is_some_and(|actual| {
                machine_replay_control_trace(expected) == machine_replay_control_trace(actual)
            })
        })
}

fn campaign_host_input_search_matches(
    expected: &CampaignSearchEvidence,
    actual: &CampaignSearchEvidence,
) -> bool {
    let mut expected = expected.clone();
    let mut actual = actual.clone();
    // Paused RAM dirtiness and guidance hashes include ungoverned kernel PCs
    // and topology-state fingerprints. Structural checkpoint work and the
    // number of recorded observations remain comparable.
    expected.checkpoint.private_dirty_pages = 0;
    actual.checkpoint.private_dirty_pages = 0;
    expected.guidance_sha256.clear();
    actual.guidance_sha256.clear();
    expected == actual
}

/// Legacy campaign results can omit the target service, stable boundary ID,
/// input receipt, or barrier receipt. Continue to verify every older field
/// while allowing only those absent additions; new results lock all of them
/// into replay verification.
fn campaign_timeline_matches(
    expected: &[CampaignTimelineBoundary],
    actual: &[CampaignTimelineBoundary],
) -> bool {
    expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(expected, actual)| {
            (expected.service.is_empty() || expected.service == actual.service) && {
                let mut normalized = actual.clone();
                normalized.service = expected.service.clone();
                if expected.id.is_empty() {
                    normalized.id.clear();
                }
                if campaign_input_is_absent(&expected.input) {
                    normalized.input = expected.input.clone();
                }
                if campaign_uart_delivery_is_absent(&expected.delivery) {
                    normalized.delivery = expected.delivery.clone();
                }
                if campaign_uart_barrier_is_absent(&expected.barrier) {
                    normalized.barrier = expected.barrier.clone();
                }
                if expected.application_blocks.is_empty() {
                    normalized.application_blocks.clear();
                    normalized.new_application_blocks.clear();
                }
                if expected.thread_synchronization.is_empty() {
                    normalized.thread_synchronization.clear();
                    normalized.new_thread_synchronization_events.clear();
                }
                if expected.structured_choices.is_empty() {
                    normalized.structured_choices.clear();
                    normalized.new_structured_choices.clear();
                }
                if expected.execution_ledgers.is_empty() {
                    normalized.execution_ledgers.clear();
                }
                if expected.machine_execution_ledgers.is_empty() {
                    normalized.machine_execution_ledgers.clear();
                }
                *expected == normalized
            }
        })
}

fn campaign_input_is_absent(input: &CampaignInputEvidence) -> bool {
    input.bytes == 0
        && input.sha256.is_empty()
        && input.excerpt.is_empty()
        && input.omitted_bytes == 0
}

fn campaign_uart_delivery_is_absent(delivery: &CampaignUartDelivery) -> bool {
    !delivery.recorded
        && delivery.accepted_bytes == 0
        && delivery.pending_before == 0
        && delivery.pending_after == 0
        && delivery.guest_read_bytes == 0
        && delivery.checkpoint.is_empty()
}

fn campaign_uart_barrier_is_absent(barrier: &CampaignUartBarrier) -> bool {
    !barrier.recorded
        && barrier.checkpoint.is_empty()
        && barrier.marker_offset == 0
        && barrier.response == CampaignSerialDelta::default()
}

/// Choose the next leaf from observed marker, paused-PC, topology-state, and
/// declared-property evidence after first seeding every one-operation history
/// without faults. An observation only guides schedules that extend the exact
/// operation history which produced it; adaptive, posterior, and property
/// guidance additionally rank a final operation by its observed yield across
/// prior contexts. Fault variants remain separate leaves and all ties fall
/// back to stable corpus order.
fn select_campaign_schedule(
    schedules: &[CampaignSchedule],
    pending: &[usize],
    observations: &[CampaignGuidanceObservation],
    guidance: CampaignGuidance,
    coverage: CampaignCoverage,
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
    let mut selected_property_witnesses = 0_usize;
    let mut selected_reason = "canonical breadth-first seed".to_owned();
    for (pending_index, schedule_index) in pending.iter().enumerate() {
        let candidate = &schedules[*schedule_index];
        let mut coverage_score = 0_usize;
        let mut coverage_reason = None;
        for observation in observations {
            if !candidate.operations.starts_with(&observation.operations) {
                continue;
            }
            let signal = campaign_guidance_signal(observation, coverage);
            let candidate_score = signal.saturating_mul(256) + observation.operations.len();
            if candidate_score > coverage_score {
                coverage_score = candidate_score;
                coverage_reason = Some(observation);
            }
        }
        let (score, property_witnesses, reason) = match guidance {
            CampaignGuidance::Coverage => (
                coverage_score,
                0,
                coverage_reason
                    .map(|observation| campaign_guidance_reason(observation, coverage))
                    .unwrap_or_else(|| "canonical breadth-first seed".to_owned()),
            ),
            CampaignGuidance::Adaptive => {
                let choice = *candidate
                    .operations
                    .last()
                    .expect("campaign schedules always contain an operation");
                let (mean_reward, observations, exploration_bonus) =
                    campaign_adaptive_action_reward(choice, observations, coverage);
                let adaptive_score = mean_reward
                    .saturating_mul(64)
                    .saturating_add(exploration_bonus);
                let base_reason = coverage_reason
                    .filter(|observation| campaign_guidance_signal(observation, coverage) > 0)
                    .map(|observation| campaign_guidance_reason(observation, coverage))
                    .unwrap_or_else(|| "canonical breadth-first seed".to_owned());
                (
                    coverage_score.saturating_add(adaptive_score),
                    0,
                    format!(
                        "{base_reason}; adaptive action reward {mean_reward} from {observations} observed run(s), exploration bonus {exploration_bonus}"
                    ),
                )
            }
            CampaignGuidance::Posterior => {
                let choice = *candidate
                    .operations
                    .last()
                    .expect("campaign schedules always contain an operation");
                let estimate = campaign_posterior_estimate(
                    choice,
                    &candidate.operations[..candidate.operations.len() - 1],
                    observations,
                    coverage,
                );
                let base_reason = coverage_reason
                    .filter(|observation| campaign_guidance_signal(observation, coverage) > 0)
                    .map(|observation| campaign_guidance_reason(observation, coverage))
                    .unwrap_or_else(|| "canonical breadth-first seed".to_owned());
                (
                    coverage_score.saturating_add(estimate.score),
                    0,
                    format!(
                        "{base_reason}; posterior {} evidence: {} yield(s), {} miss(es), mean {}‰, uncertainty {}‰",
                        estimate.scope,
                        estimate.successes,
                        estimate.misses,
                        estimate.mean_per_mille,
                        estimate.uncertainty_per_mille,
                    ),
                )
            }
            CampaignGuidance::Property => {
                let choice = *candidate
                    .operations
                    .last()
                    .expect("campaign schedules always contain an operation");
                let estimate = campaign_property_estimate(
                    choice,
                    &candidate.operations[..candidate.operations.len() - 1],
                    observations,
                );
                let base_reason = coverage_reason
                    .filter(|observation| campaign_guidance_signal(observation, coverage) > 0)
                    .map(|observation| campaign_guidance_reason(observation, coverage))
                    .unwrap_or_else(|| "canonical breadth-first seed".to_owned());
                let properties = if estimate.properties.is_empty() {
                    "no property witness yet".to_owned()
                } else {
                    estimate.properties.join(", ")
                };
                (
                    coverage_score.saturating_add(estimate.score),
                    estimate.witnesses,
                    format!(
                        "{base_reason}; property {} evidence: {properties}; {} witness(es), {} miss(es), exploration bonus {}",
                        estimate.scope,
                        estimate.witnesses,
                        estimate.misses,
                        estimate.exploration_bonus,
                    ),
                )
            }
            CampaignGuidance::Unified => {
                let prefix = campaign_schedule_decision_prefix(candidate);
                let related = observations
                    .iter()
                    .map(|observation| {
                        (
                            common_decision_prefix(&prefix, &observation.decision_prefix),
                            observation,
                        )
                    })
                    .max_by_key(|(shared, observation)| {
                        (*shared, campaign_unified_guidance_signal(observation))
                    });
                let (shared, observed_signal, observed_properties) =
                    related.map_or((0, 0, 0), |(shared, observation)| {
                        (
                            shared,
                            campaign_unified_guidance_signal(observation),
                            observation.property_witnesses.len(),
                        )
                    });
                let visits = observations
                    .iter()
                    .filter(|observation| {
                        common_decision_prefix(&prefix, &observation.decision_prefix) >= shared
                    })
                    .count();
                let exploration_bonus = observations.len().saturating_add(1).saturating_mul(1_000)
                    / visits.saturating_add(1);
                (
                    coverage_score
                        .saturating_add(observed_signal.saturating_mul(256))
                        .saturating_add(shared.saturating_mul(64))
                        .saturating_add(exploration_bonus),
                    observed_properties,
                    format!(
                        "unified decision prefix shares {shared} point(s); observed reward {observed_signal}; exploration bonus {exploration_bonus}"
                    ),
                )
            }
        };
        if property_witnesses > selected_property_witnesses
            || (property_witnesses == selected_property_witnesses && score > selected_score)
        {
            selected = pending_index;
            selected_score = score;
            selected_property_witnesses = property_witnesses;
            selected_reason = reason;
        }
    }
    (selected, selected_reason)
}

fn campaign_unified_guidance_signal(observation: &CampaignGuidanceObservation) -> usize {
    observation
        .novel_markers
        .saturating_mul(1_000)
        .saturating_add(observation.novel_instructions.saturating_mul(500))
        .saturating_add(observation.novel_checkpoint_pcs.saturating_mul(500))
        .saturating_add(observation.novel_application_blocks.saturating_mul(1_000))
        .saturating_add(observation.novel_structured_choices.saturating_mul(2_000))
        .saturating_add(observation.novel_scheduling_decisions.saturating_mul(2_000))
        .saturating_add(usize::from(observation.novel_state).saturating_mul(2_000))
        .saturating_add(observation.property_witnesses.len().saturating_mul(100_000))
        .saturating_add(usize::from(observation.failed).saturating_mul(10_000))
}

fn campaign_guidance_signal(
    observation: &CampaignGuidanceObservation,
    coverage: CampaignCoverage,
) -> usize {
    let primary = match coverage {
        CampaignCoverage::Markers => observation.novel_markers.saturating_mul(1_000),
        CampaignCoverage::CheckpointPcs => observation.novel_checkpoint_pcs.saturating_mul(500),
        CampaignCoverage::ExecutionLocations => observation.novel_instructions.saturating_mul(500),
        CampaignCoverage::ApplicationBlocks | CampaignCoverage::ApplicationEdges => {
            observation.novel_application_blocks.saturating_mul(1_000)
        }
    };
    primary
        .saturating_add(usize::from(observation.novel_state).saturating_mul(250))
        .saturating_add(usize::from(observation.failed).saturating_mul(100))
}

/// Return a deterministic empirical action reward and an uncertainty bonus.
/// The bonus declines with observations, so bounded campaigns still sample a
/// little-used operation instead of permanently repeating an early winner.
fn campaign_adaptive_action_reward(
    choice: CampaignOperationChoice,
    observations: &[CampaignGuidanceObservation],
    coverage: CampaignCoverage,
) -> (usize, usize, usize) {
    let matching = observations
        .iter()
        .filter(|observation| observation.operations.last() == Some(&choice))
        .collect::<Vec<_>>();
    let count = matching.len();
    let reward = matching
        .into_iter()
        .map(|observation| campaign_guidance_signal(observation, coverage))
        .sum::<usize>();
    let mean_reward = reward / count.max(1);
    let exploration_bonus =
        observations.len().saturating_add(1).saturating_mul(250) / count.saturating_add(1);
    (mean_reward, count, exploration_bonus)
}

#[derive(Debug, Clone, Copy)]
struct CampaignPosteriorEstimate {
    scope: &'static str,
    successes: usize,
    misses: usize,
    mean_per_mille: usize,
    uncertainty_per_mille: usize,
    score: usize,
}

/// A deterministic action estimate for property-directed scheduling. A
/// witness is a reachable/sometimes match or an always/unreachable
/// counterexample. The exploration bonus keeps unseen actions eligible when
/// no operation has produced a witness yet.
struct CampaignPropertyEstimate {
    scope: &'static str,
    witnesses: usize,
    misses: usize,
    properties: Vec<String>,
    exploration_bonus: usize,
    score: usize,
}

fn campaign_property_estimate(
    choice: CampaignOperationChoice,
    context: &[CampaignOperationChoice],
    observations: &[CampaignGuidanceObservation],
) -> CampaignPropertyEstimate {
    let contextual = observations
        .iter()
        .filter(|observation| {
            observation.operations.last() == Some(&choice)
                && observation.operations[..observation.operations.len().saturating_sub(1)]
                    == *context
        })
        .collect::<Vec<_>>();
    let (scope, matching) = if contextual.is_empty() {
        (
            "global action",
            observations
                .iter()
                .filter(|observation| observation.operations.last() == Some(&choice))
                .collect::<Vec<_>>(),
        )
    } else {
        ("exact context", contextual)
    };
    let witnesses = matching
        .iter()
        .filter(|observation| !observation.property_witnesses.is_empty())
        .count();
    let misses = matching.len().saturating_sub(witnesses);
    let properties = matching
        .iter()
        .flat_map(|observation| observation.property_witnesses.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let exploration_bonus =
        observations.len().saturating_add(1).saturating_mul(500) / matching.len().saturating_add(1);
    CampaignPropertyEstimate {
        scope,
        witnesses,
        misses,
        properties,
        exploration_bonus,
        score: witnesses
            .saturating_mul(100_000)
            .saturating_add(exploration_bonus),
    }
}

/// Rank an action with a uniform Beta(1, 1) prior and a deterministic
/// uncertainty width. Prefer evidence from the exact preceding operation
/// history; when that history has not tried the action, fall back to the
/// action's global evidence. This stays reproducible and never samples a
/// random posterior.
fn campaign_posterior_estimate(
    choice: CampaignOperationChoice,
    context: &[CampaignOperationChoice],
    observations: &[CampaignGuidanceObservation],
    coverage: CampaignCoverage,
) -> CampaignPosteriorEstimate {
    let contextual = observations
        .iter()
        .filter(|observation| {
            observation.operations.last() == Some(&choice)
                && observation.operations[..observation.operations.len().saturating_sub(1)]
                    == *context
        })
        .collect::<Vec<_>>();
    let (scope, matching) = if contextual.is_empty() {
        (
            "global action",
            observations
                .iter()
                .filter(|observation| observation.operations.last() == Some(&choice))
                .collect::<Vec<_>>(),
        )
    } else {
        ("exact context", contextual)
    };
    let successes = matching
        .iter()
        .filter(|observation| campaign_guidance_signal(observation, coverage) > 0)
        .count();
    let misses = matching.len().saturating_sub(successes);
    let alpha = successes.saturating_add(1);
    let beta = misses.saturating_add(1);
    let trials = alpha.saturating_add(beta);
    let mean_per_mille = alpha.saturating_mul(1_000) / trials;
    // A small evidence-width bonus gives unseen actions a fair deterministic
    // trial but declines as the posterior accumulates observations.
    let uncertainty_per_mille = 500 / trials;
    let upper_per_mille = mean_per_mille
        .saturating_add(uncertainty_per_mille)
        .min(1_000);
    CampaignPosteriorEstimate {
        scope,
        successes,
        misses,
        mean_per_mille,
        uncertainty_per_mille,
        score: upper_per_mille.saturating_mul(64),
    }
}

fn campaign_posterior_evidence(
    campaign: &CampaignPlan,
    schedule: &CampaignSchedule,
    observations: &[CampaignGuidanceObservation],
) -> CampaignPosteriorEvidence {
    let choice = *schedule
        .operations
        .last()
        .expect("campaign schedules always contain an operation");
    let context = &schedule.operations[..schedule.operations.len() - 1];
    let estimate = campaign_posterior_estimate(choice, context, observations, campaign.coverage);
    CampaignPosteriorEvidence {
        action: campaign_operation_choice_name(campaign, choice),
        context: context
            .iter()
            .map(|choice| campaign_operation_choice_name(campaign, *choice))
            .collect(),
        scope: estimate.scope.to_owned(),
        successes: estimate.successes,
        misses: estimate.misses,
        mean_per_mille: estimate.mean_per_mille,
        uncertainty_per_mille: estimate.uncertainty_per_mille,
        score: estimate.score,
    }
}

fn campaign_guidance_reason(
    observation: &CampaignGuidanceObservation,
    coverage: CampaignCoverage,
) -> String {
    let mut signals = Vec::new();
    if coverage == CampaignCoverage::Markers && observation.novel_markers > 0 {
        signals.push(format!("{} new marker(s)", observation.novel_markers));
    }
    if coverage == CampaignCoverage::ExecutionLocations && observation.novel_instructions > 0 {
        signals.push(format!(
            "{} new instruction location(s)",
            observation.novel_instructions
        ));
    }
    if matches!(
        coverage,
        CampaignCoverage::ApplicationBlocks | CampaignCoverage::ApplicationEdges
    ) && observation.novel_application_blocks > 0
    {
        signals.push(format!(
            "{} new application {}(s)",
            observation.novel_application_blocks,
            if coverage == CampaignCoverage::ApplicationEdges {
                "edge"
            } else {
                "block"
            }
        ));
    }
    if coverage == CampaignCoverage::CheckpointPcs && observation.novel_checkpoint_pcs > 0 {
        signals.push(format!(
            "{} new checkpoint PC(s)",
            observation.novel_checkpoint_pcs
        ));
    }
    if observation.novel_state {
        signals.push("new topology state".to_owned());
    }
    if observation.failed {
        signals.push("failed run".to_owned());
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
        if output.len() == MAX_CAMPAIGN_CANDIDATES {
            return;
        }
        if !selected.is_empty() {
            output.push(selected.clone());
        }
        if selected.len() == maximum || output.len() == MAX_CAMPAIGN_CANDIDATES {
            return;
        }
        for (offset, index) in applicable.iter().enumerate().skip(start) {
            if output.len() == MAX_CAMPAIGN_CANDIDATES {
                return;
            }
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
    let service_action = |fault: &CampaignFault| {
        matches!(
            fault.kind,
            CampaignFaultKind::ServiceStop
                | CampaignFaultKind::ServiceStart
                | CampaignFaultKind::ServiceKill
                | CampaignFaultKind::ServiceRestart
        )
    };
    if service_action(first) && service_action(second) && first.service == second.service {
        return false;
    }
    let throttle = |fault: &CampaignFault| matches!(fault.kind, CampaignFaultKind::CpuThrottle);
    if throttle(first) && throttle(second) && first.service == second.service {
        return false;
    }
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
    history: &[CampaignOperationChoice],
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
        | CampaignFaultKind::LinkClog
        | CampaignFaultKind::LinkUnclog
        | CampaignFaultKind::CpuThrottle
        | CampaignFaultKind::CpuRelease
        | CampaignFaultKind::LinkFault
        | CampaignFaultKind::LinkRecover
        | CampaignFaultKind::ServiceStop
        | CampaignFaultKind::ServiceStart
        | CampaignFaultKind::ServiceKill
        | CampaignFaultKind::ServiceRestart
        | CampaignFaultKind::StorageFault
        | CampaignFaultKind::StorageRecover
        | CampaignFaultKind::NetworkFault
        | CampaignFaultKind::NetworkRecover
        | CampaignFaultKind::PacketFault
        | CampaignFaultKind::PacketRecover => {
            if fault.after.is_none() && fault.after_input.is_none() {
                return false;
            }
            history
                .iter()
                .any(|operation| campaign_fault_matches_operation(campaign, fault, *operation))
        }
    }
}

fn campaign_required_faults_apply(campaign: &CampaignPlan, schedule: &CampaignSchedule) -> bool {
    schedule.faults.iter().all(|index| {
        let fault = &campaign.faults[*index];
        !fault.required || campaign_fault_applies(fault, &schedule.operations, campaign)
    })
}

fn apply_campaign_schedule(
    topology: &mut TopologyPlan,
    campaign: &CampaignPlan,
    schedule: &CampaignSchedule,
    events: &[CampaignEvent],
) -> Result<(), String> {
    let selected = schedule
        .faults
        .iter()
        .map(|index| &campaign.faults[*index])
        .collect::<Vec<_>>();
    for service in topology.services.values_mut() {
        service.run.events.clear();
    }
    topology.event_order.clear();
    for campaign_event in events {
        if !campaign_event.recover_faults.is_empty() {
            let service = topology
                .services
                .get_mut(&campaign_event.service)
                .ok_or_else(|| {
                    format!(
                        "campaign operation service disappeared: {}",
                        campaign_event.service
                    )
                })?;
            let mut quiet = campaign_shell_termination_event(&campaign_event.service);
            quiet.actions = campaign_event.recover_faults.clone();
            service.run.events.push(quiet);
            topology.event_order.push(campaign_event.service.clone());
        }
        for service_name in &campaign_event.terminate_shell_processes {
            if !campaign_event.recover_faults.is_empty() && service_name == &campaign_event.service
            {
                continue;
            }
            let service = topology.services.get_mut(service_name).ok_or_else(|| {
                format!("eventually termination service disappeared: {service_name}")
            })?;
            service
                .run
                .events
                .push(campaign_shell_termination_event(service_name));
            topology.event_order.push(service_name.clone());
        }
        let service = topology
            .services
            .get_mut(&campaign_event.service)
            .ok_or_else(|| {
                format!(
                    "campaign operation service disappeared: {}",
                    campaign_event.service
                )
            })?;
        service.run.events.push(campaign_event.event.clone());
        topology.event_order.push(campaign_event.service.clone());
    }
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

/// A recorded replay plan retains the mixed-service input corpus for people
/// and offline reports. Restored campaign checkpoints already include those
/// inputs, so execution must clear every service's plan-level event list.
fn clear_campaign_events(topology: &mut TopologyPlan) {
    for service in topology.services.values_mut() {
        service.run.events.clear();
    }
    topology.event_order.clear();
}

/// Restore the global event stream recorded by a campaign export. Legacy and
/// hand-written plans without an order retain the original service-grouped
/// behavior.
fn ordered_topology_events(topology: &TopologyPlan) -> Result<Vec<(String, EventPlan)>, String> {
    if topology.event_order.is_empty() {
        return Ok(topology
            .services
            .iter()
            .flat_map(|(name, service)| {
                service
                    .run
                    .events
                    .iter()
                    .cloned()
                    .map(|event| (name.clone(), event))
            })
            .collect());
    }

    let mut positions = BTreeMap::<String, usize>::new();
    let mut ordered = Vec::with_capacity(topology.event_order.len());
    for name in &topology.event_order {
        let service = topology
            .services
            .get(name)
            .ok_or_else(|| format!("event_order names unknown service {name}"))?;
        let position = positions.entry(name.clone()).or_default();
        let event = service
            .run
            .events
            .get(*position)
            .ok_or_else(|| format!("event_order has too many entries for service {name}"))?;
        ordered.push((name.clone(), event.clone()));
        *position += 1;
    }
    for (name, service) in &topology.services {
        let included = positions.get(name).copied().unwrap_or_default();
        if included != service.run.events.len() {
            return Err(format!(
                "event_order includes {included} of {} events for service {name}",
                service.run.events.len()
            ));
        }
    }
    Ok(ordered)
}

fn campaign_schedule_event(
    campaign: &CampaignPlan,
    schedule: &CampaignSchedule,
    index: usize,
    checkpoint: &CampaignCheckpoint,
) -> Result<CampaignEvent, String> {
    let selected = schedule
        .faults
        .iter()
        .map(|index| &campaign.faults[*index])
        .collect::<Vec<_>>();
    let operation = *schedule
        .operations
        .get(index)
        .ok_or_else(|| format!("campaign schedule has no operation at index {index}"))?;
    let definition = &campaign.operations[operation.operation];
    let input = campaign_operation_input(campaign, operation)?;
    let actions = selected
        .iter()
        .filter(|candidate| campaign_fault_matches_operation(campaign, candidate, operation))
        .map(|candidate| campaign_action(candidate))
        .collect::<Result<Vec<_>, _>>()?;
    let mut data_hex = campaign_operation_input_hex(campaign, definition, checkpoint, &input)?;
    if let Some(exploration) = &definition.thread_schedule_exploration {
        let prefix = schedule
            .thread_schedule_prefixes
            .get(index)
            .ok_or_else(|| "campaign schedule is missing a runnable prefix".to_owned())?;
        if prefix.len() > usize::from(exploration.max_choices)
            || prefix.iter().any(|thread| *thread >= 32)
        {
            return Err("campaign schedule has an invalid runnable prefix".to_owned());
        }
        data_hex = campaign_input_with_runnable_prefix(&data_hex, prefix)?;
    }
    let terminal_eventually = definition.command == Some(CampaignTestCommand::Eventually);
    let quiet_terminal = matches!(
        definition.command,
        Some(CampaignTestCommand::Eventually | CampaignTestCommand::Finally)
    );
    let recover_faults = if quiet_terminal {
        selected
            .iter()
            .filter(|fault| campaign_fault_applies(fault, &schedule.operations[..index], campaign))
            .filter_map(|fault| campaign_recovery_action(fault, &definition.name).transpose())
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok(CampaignEvent {
        service: campaign_operation_service(campaign, operation).to_owned(),
        terminate_shell_processes: if terminal_eventually {
            campaign_active_shell_process_services(campaign, &schedule.operations[..index])
        } else {
            Vec::new()
        },
        recover_faults,
        event: EventPlan {
            data_hex,
            checkpoint: Some(format!("THES:CHECKPOINT:{}", definition.name)),
            actions,
        },
    })
}

fn campaign_input_with_runnable_prefix(input_hex: &str, prefix: &[u8]) -> Result<String, String> {
    let bytes = decode_hex(input_hex)?;
    let command = std::str::from_utf8(&bytes)
        .map_err(|error| format!("runnable-prefix command is not UTF-8: {error}"))?
        .strip_prefix("THES:SHELL:operation:")
        .and_then(|command| command.strip_suffix('\n'))
        .ok_or_else(|| "runnable-prefix exploration requires a shell operation".to_owned())?;
    let mut command: serde_json::Value = serde_json::from_str(command)
        .map_err(|error| format!("runnable-prefix command is not valid JSON: {error}"))?;
    let environment = command
        .get_mut("environment")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| "runnable-prefix shell operation has no environment".to_owned())?;
    if environment
        .get("THESEUS_THREAD_SCHEDULE_MODE")
        .and_then(serde_json::Value::as_str)
        != Some("runnable_prefix")
    {
        return Err("runnable-prefix shell operation has the wrong scheduler mode".to_owned());
    }
    environment.insert(
        "THESEUS_THREAD_SCHEDULE".to_owned(),
        serde_json::Value::String(
            prefix
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
    );
    let command = serde_json::to_string(&command)
        .map_err(|error| format!("cannot encode runnable-prefix command: {error}"))?;
    Ok(hex(format!("THES:SHELL:operation:{command}\n").as_bytes()))
}

fn campaign_operation_input_hex(
    campaign: &CampaignPlan,
    operation: &CampaignOperation,
    checkpoint: &CampaignCheckpoint,
    input: &CampaignOperationInput,
) -> Result<String, String> {
    validate_campaign_thread_schedule_input(input)?;
    let Some(template) = &input.input_template else {
        return Ok(input.input_hex.clone());
    };
    let mut values = BTreeMap::new();
    for (name, capture) in &input.input_captures {
        let service = capture
            .service
            .as_deref()
            .unwrap_or(if operation.service.is_empty() {
                &campaign.driver
            } else {
                &operation.service
            });
        let serial = campaign_checkpoint_serial(checkpoint, service);
        let mut captured_values = campaign_input_capture_values(checkpoint, &serial, capture)
            .into_iter()
            .filter_map(|value| campaign_input_value(value, capture.encoding));
        let value = match capture.select {
            CampaignOperationInputSelect::First => captured_values.next(),
            CampaignOperationInputSelect::Latest => captured_values.last(),
        }
            .ok_or_else(|| {
                format!(
                    "campaign input capture {name:?} found no usable value at {:?} in service {service:?}",
                    capture.pointer
                )
            })?;
        values.insert(name.as_str(), value);
    }
    let mut rendered = String::new();
    let mut cursor = 0;
    while let Some(relative_start) = template[cursor..].find('{') {
        let start = cursor + relative_start;
        rendered.push_str(&template[cursor..start]);
        let value_start = start + 1;
        let end = value_start
            + template[value_start..]
                .find('}')
                .ok_or_else(|| "campaign input template was not normalized".to_owned())?;
        let variable = &template[value_start..end];
        let value = values.get(variable).ok_or_else(|| {
            format!("campaign input template has no captured value for {variable:?}")
        })?;
        rendered.push_str(value);
        cursor = end + 1;
    }
    rendered.push_str(&template[cursor..]);
    Ok(hex(rendered.as_bytes()))
}

fn validate_campaign_thread_schedule_input(input: &CampaignOperationInput) -> Result<(), String> {
    if input.thread_schedule.is_empty() {
        return Ok(());
    }
    let bytes = decode_hex(&input.input_hex)?;
    let command = std::str::from_utf8(&bytes)
        .map_err(|error| format!("thread schedule command is not UTF-8: {error}"))?
        .strip_prefix("THES:SHELL:operation:")
        .and_then(|command| command.strip_suffix('\n'))
        .ok_or_else(|| "thread schedule case is not a shell operation".to_owned())?;
    let command: serde_json::Value = serde_json::from_str(command)
        .map_err(|error| format!("thread schedule command is not valid JSON: {error}"))?;
    let actual = command
        .get("environment")
        .and_then(|environment| environment.get("THESEUS_THREAD_SCHEDULE"))
        .and_then(serde_json::Value::as_str);
    let expected = input
        .thread_schedule
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if actual != Some(expected.as_str()) {
        return Err(format!(
            "thread schedule case {:?} does not match its locked shell environment",
            input.name
        ));
    }
    Ok(())
}

fn campaign_input_capture_values(
    checkpoint: &CampaignCheckpoint,
    serial: &[u8],
    capture: &CampaignOperationInputCapture,
) -> Vec<serde_json::Value> {
    if let Some(predicate) = &capture.json {
        return serial
            .split_inclusive(|byte| *byte == b'\n')
            .filter_map(|line| {
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                let event = serde_json::from_slice::<serde_json::Value>(line).ok()?;
                json_predicate_matches(&event, predicate)
                    .then(|| event.pointer(&capture.pointer).cloned())
                    .flatten()
            })
            .collect();
    }
    if let Some(workflow) = &capture.workflow {
        return campaign_workflow_capture_values(checkpoint, workflow, &capture.pointer);
    }
    serial_sequence_capture_values(serial, &capture.sequence, &capture.pointer)
}

fn campaign_input_value(
    value: serde_json::Value,
    encoding: CampaignOperationInputEncoding,
) -> Option<String> {
    if matches!(encoding, CampaignOperationInputEncoding::Json) {
        return serde_json::to_string(&value).ok();
    }
    let text = match value {
        serde_json::Value::String(value) => value,
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            return None;
        }
    };
    Some(match encoding {
        CampaignOperationInputEncoding::Text => text,
        CampaignOperationInputEncoding::Hex => hex(text.as_bytes()),
        CampaignOperationInputEncoding::Json => unreachable!(),
    })
}

fn campaign_action(fault: &CampaignFault) -> Result<CampaignAction, String> {
    let operation = campaign_fault_barrier_name(fault);
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
        duration_rounds: fault.duration_rounds,
        every_n_rounds: fault.every_n_rounds,
    })
}

fn campaign_recovery_action(
    fault: &CampaignFault,
    operation: &str,
) -> Result<Option<CampaignAction>, String> {
    let kind = match fault.kind {
        CampaignFaultKind::Partition => CampaignFaultKind::Heal,
        CampaignFaultKind::LinkPartition => CampaignFaultKind::LinkHeal,
        CampaignFaultKind::LinkFault => CampaignFaultKind::LinkRecover,
        CampaignFaultKind::CpuThrottle => CampaignFaultKind::CpuRelease,
        CampaignFaultKind::LinkClog => CampaignFaultKind::LinkUnclog,
        CampaignFaultKind::ServiceStop | CampaignFaultKind::ServiceKill => {
            CampaignFaultKind::ServiceStart
        }
        CampaignFaultKind::StorageFault => CampaignFaultKind::StorageRecover,
        CampaignFaultKind::NetworkFault => CampaignFaultKind::NetworkRecover,
        CampaignFaultKind::PacketFault => CampaignFaultKind::PacketRecover,
        CampaignFaultKind::Pause
        | CampaignFaultKind::Restart
        | CampaignFaultKind::ClockJump
        | CampaignFaultKind::CpuRelease
        | CampaignFaultKind::Heal
        | CampaignFaultKind::LinkHeal
        | CampaignFaultKind::LinkUnclog
        | CampaignFaultKind::LinkRecover
        | CampaignFaultKind::ServiceStart
        | CampaignFaultKind::ServiceRestart
        | CampaignFaultKind::StorageRecover
        | CampaignFaultKind::NetworkRecover
        | CampaignFaultKind::PacketRecover => return Ok(None),
    };
    let mut action = campaign_action(fault)?;
    action.operation = operation.to_owned();
    action.kind = kind;
    Ok(Some(action))
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
            campaign_fault_barrier_name(fault)
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
            campaign_fault_barrier_name(fault)
        ),
        CampaignFaultKind::LinkClog | CampaignFaultKind::LinkUnclog => format!(
            "{}:{}->{}:{}@{}",
            fault.network.as_deref().expect("validated action network"),
            fault.from.as_deref().expect("validated action source"),
            fault.to.as_deref().expect("validated action destination"),
            if matches!(fault.kind, CampaignFaultKind::LinkClog) {
                "link_clog"
            } else {
                "link_unclog"
            },
            campaign_fault_barrier_name(fault)
        ),
        CampaignFaultKind::LinkFault | CampaignFaultKind::LinkRecover => format!(
            "{}:{}->{}:{}@{}",
            fault.network.as_deref().expect("validated action network"),
            fault.from.as_deref().expect("validated action source"),
            fault.to.as_deref().expect("validated action destination"),
            if matches!(fault.kind, CampaignFaultKind::LinkFault) {
                "link_fault"
            } else {
                "link_recover"
            },
            campaign_fault_barrier_name(fault)
        ),
        CampaignFaultKind::CpuThrottle | CampaignFaultKind::CpuRelease => format!(
            "{}:{}@{}",
            fault.service.as_deref().expect("validated service target"),
            match fault.kind {
                CampaignFaultKind::CpuThrottle => "cpu_throttle",
                CampaignFaultKind::CpuRelease => "cpu_release",
                _ => unreachable!(),
            },
            campaign_fault_barrier_name(fault)
        ),
        CampaignFaultKind::ServiceStop
        | CampaignFaultKind::ServiceStart
        | CampaignFaultKind::ServiceKill
        | CampaignFaultKind::ServiceRestart => format!(
            "{}:{}@{}",
            fault.service.as_deref().expect("validated service target"),
            match fault.kind {
                CampaignFaultKind::ServiceStop => "service_stop",
                CampaignFaultKind::ServiceStart => "service_start",
                CampaignFaultKind::ServiceKill => "service_kill",
                CampaignFaultKind::ServiceRestart => "service_restart",
                _ => unreachable!(),
            },
            campaign_fault_barrier_name(fault)
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
            campaign_fault_barrier_name(fault)
        ),
        CampaignFaultKind::NetworkFault | CampaignFaultKind::NetworkRecover => format!(
            "{}:{}@{}",
            fault.network.as_deref().expect("validated action network"),
            match fault.kind {
                CampaignFaultKind::NetworkFault => "network_fault",
                CampaignFaultKind::NetworkRecover => "network_recover",
                _ => unreachable!(),
            },
            campaign_fault_barrier_name(fault)
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
                campaign_fault_barrier_name(fault)
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

const APPLICATION_BLOCK_COVERAGE_PREFIX: &str = "THES:COV:v1:";
const APPLICATION_EDGE_COVERAGE_PREFIX: &str = "THES:COV:v2:";
const APPLICATION_EDGE_COVERAGE_LIMIT: u32 = 65_536;

fn campaign_application_blocks(
    run: &Path,
) -> Result<BTreeMap<String, Vec<ApplicationBlock>>, String> {
    Ok(campaign_application_blocks_from_serial(
        &campaign_serial_logs(run)?,
    ))
}

fn campaign_serial_logs(run: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut serial = BTreeMap::<String, Vec<u8>>::new();
    let services = fs::read_dir(run.join("services")).map_err(|error| error.to_string())?;
    for service in services {
        let service = service.map_err(|error| error.to_string())?;
        let name = service.file_name().to_string_lossy().into_owned();
        let mut contents = Vec::new();
        let mut logs = fs::read_dir(service.path())
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        logs.sort_by_key(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix("serial-")
                .and_then(|suffix| suffix.strip_suffix(".log"))
                .and_then(|index| index.parse::<usize>().ok())
                .map(|index| index + 1)
                .unwrap_or(0)
        });
        for log in logs {
            let file_name = log.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name == "serial.log"
                || (file_name.starts_with("serial-") && file_name.ends_with(".log"))
            {
                contents.extend(fs::read(log.path()).unwrap_or_default());
                contents.push(b'\n');
            }
        }
        serial.insert(name, contents);
    }
    Ok(serial)
}

fn campaign_application_blocks_from_serial(
    serial: &BTreeMap<String, Vec<u8>>,
) -> BTreeMap<String, Vec<ApplicationBlock>> {
    serial
        .iter()
        .filter_map(|(service, contents)| {
            let blocks = String::from_utf8_lossy(contents)
                .lines()
                .filter_map(parse_application_coverage_line)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            (!blocks.is_empty()).then(|| (service.clone(), blocks))
        })
        .collect()
}

fn parse_application_coverage_line(line: &str) -> Option<ApplicationBlock> {
    let line = line.trim();
    let (record, edge) = if let Some(record) = line.strip_prefix(APPLICATION_BLOCK_COVERAGE_PREFIX)
    {
        (record, None)
    } else {
        let mut fields = line
            .strip_prefix(APPLICATION_EDGE_COVERAGE_PREFIX)?
            .split(':');
        let process = fields.next()?;
        let module = fields.next()?;
        let build_sha256 = fields.next()?;
        let edge = fields.next()?.parse::<u32>().ok()?;
        let offset = fields.next()?;
        if edge == 0
            || edge >= APPLICATION_EDGE_COVERAGE_LIMIT
            || fields.next().is_some()
            || !valid_application_coverage_point(process, module, build_sha256, offset)
        {
            return None;
        }
        return Some(ApplicationBlock {
            process: process.to_owned(),
            module: module.to_owned(),
            build_sha256: build_sha256.to_owned(),
            edge: Some(edge),
            offset: offset.to_owned(),
            symbol: None,
            symbol_offset: None,
            source: None,
        });
    };
    let mut fields = record.split(':');
    let process = fields.next()?;
    let module = fields.next()?;
    let build_sha256 = fields.next()?;
    let offset = fields.next()?;
    if fields.next().is_some()
        || !valid_application_coverage_point(process, module, build_sha256, offset)
    {
        return None;
    }
    Some(ApplicationBlock {
        process: process.to_owned(),
        module: module.to_owned(),
        build_sha256: build_sha256.to_owned(),
        edge,
        offset: offset.to_owned(),
        symbol: None,
        symbol_offset: None,
        source: None,
    })
}

fn valid_application_coverage_point(
    process: &str,
    module: &str,
    build_sha256: &str,
    offset: &str,
) -> bool {
    valid_coverage_name(process)
        && valid_coverage_name(module)
        && build_sha256.len() == 64
        && build_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && valid_coverage_offset(offset)
}

fn valid_coverage_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_coverage_offset(value: &str) -> bool {
    value.strip_prefix("0x").is_some_and(|hex| {
        !hex.is_empty()
            && hex.len() <= 16
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn campaign_application_block_id(service: &str, block: &ApplicationBlock) -> String {
    let point = block
        .edge
        .map(|edge| format!("edge-{edge}:{}", block.offset))
        .unwrap_or_else(|| block.offset.clone());
    format!(
        "{service}:{}:{}@{}:{point}",
        block.process, block.module, block.build_sha256
    )
}

fn campaign_application_block_ids(blocks: &BTreeMap<String, Vec<ApplicationBlock>>) -> Vec<String> {
    blocks
        .iter()
        .flat_map(|(service, blocks)| {
            blocks
                .iter()
                .map(move |block| campaign_application_block_id(service, block))
        })
        .collect()
}

const THREAD_SCHEDULING_PREFIX: &str = "THES:SCHED:v1:";

fn campaign_thread_scheduling(
    run: &Path,
) -> Result<BTreeMap<String, Vec<ThreadSchedulingDecision>>, String> {
    Ok(campaign_thread_scheduling_from_serial(
        &campaign_serial_logs(run)?,
    ))
}

fn campaign_thread_scheduling_from_serial(
    serial: &BTreeMap<String, Vec<u8>>,
) -> BTreeMap<String, Vec<ThreadSchedulingDecision>> {
    serial
        .iter()
        .filter_map(|(service, contents)| {
            let decisions = String::from_utf8_lossy(contents)
                .lines()
                .filter_map(parse_thread_scheduling_line)
                .collect::<Vec<_>>();
            (!decisions.is_empty()).then(|| (service.clone(), decisions))
        })
        .collect()
}

fn parse_thread_scheduling_line(line: &str) -> Option<ThreadSchedulingDecision> {
    let mut fields = line
        .trim()
        .strip_prefix(THREAD_SCHEDULING_PREFIX)?
        .split(':');
    let process = fields.next()?;
    let module = fields.next()?;
    let build_sha256 = fields.next()?;
    let decision = fields.next()?.parse::<u64>().ok()?;
    let from_thread = fields.next()?.parse::<u8>().ok()?;
    let runnable_mask = fields.next()?;
    let selected_thread = fields.next()?.parse::<u8>().ok()?;
    let point_offset = fields.next()?;
    let mask = u32::from_str_radix(runnable_mask.strip_prefix("0x")?, 16).ok()?;
    if fields.next().is_some()
        || !valid_coverage_name(process)
        || !valid_coverage_name(module)
        || build_sha256.len() != 64
        || !build_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || runnable_mask.len() != 10
        || !runnable_mask[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || from_thread >= 32
        || selected_thread >= 32
        || mask & (1_u32 << selected_thread) == 0
        || !valid_coverage_offset(point_offset)
    {
        return None;
    }
    Some(ThreadSchedulingDecision {
        process: process.to_owned(),
        module: module.to_owned(),
        build_sha256: build_sha256.to_owned(),
        decision,
        from_thread,
        runnable_mask: runnable_mask.to_owned(),
        selected_thread,
        point_offset: point_offset.to_owned(),
    })
}

const THREAD_SYNCHRONIZATION_PREFIX: &str = "THES:SYNC:v1:";

fn campaign_thread_synchronization(
    run: &Path,
) -> Result<BTreeMap<String, Vec<ThreadSynchronizationEvent>>, String> {
    Ok(campaign_thread_synchronization_from_serial(
        &campaign_serial_logs(run)?,
    ))
}

fn campaign_thread_synchronization_from_serial(
    serial: &BTreeMap<String, Vec<u8>>,
) -> BTreeMap<String, Vec<ThreadSynchronizationEvent>> {
    serial
        .iter()
        .filter_map(|(service, contents)| {
            let events = String::from_utf8_lossy(contents)
                .lines()
                .filter_map(parse_thread_synchronization_line)
                .collect::<Vec<_>>();
            (!events.is_empty()).then(|| (service.clone(), events))
        })
        .collect()
}

fn parse_thread_synchronization_line(line: &str) -> Option<ThreadSynchronizationEvent> {
    let mut fields = line
        .trim()
        .strip_prefix(THREAD_SYNCHRONIZATION_PREFIX)?
        .split(':');
    let process = fields.next()?;
    let module = fields.next()?;
    let build_sha256 = fields.next()?;
    let event = fields.next()?.parse::<u64>().ok()?;
    let thread = fields.next()?.parse::<u8>().ok()?;
    let operation = fields.next()?;
    let object_kind = fields.next()?;
    let object = fields.next()?.parse::<u16>().ok()?;
    let peer = fields.next()?;
    let peer_thread = if peer == "-" {
        None
    } else {
        Some(peer.parse::<u8>().ok()?)
    };
    if fields.next().is_some()
        || !valid_coverage_name(process)
        || !valid_coverage_name(module)
        || build_sha256.len() != 64
        || !build_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || thread >= 32
        || peer_thread.is_some_and(|peer| peer >= 32)
        || !matches!(
            operation,
            "acquire" | "wait" | "release" | "release-for-wait" | "resume" | "signal" | "broadcast"
        )
        || !matches!(object_kind, "mutex" | "condition")
        || !matches!(
            (operation, object_kind),
            ("acquire" | "wait" | "release" | "release-for-wait", "mutex")
                | ("wait" | "resume" | "signal" | "broadcast", "condition")
        )
        || object >= 128
    {
        return None;
    }
    Some(ThreadSynchronizationEvent {
        process: process.to_owned(),
        module: module.to_owned(),
        build_sha256: build_sha256.to_owned(),
        event,
        thread,
        operation: operation.to_owned(),
        object_kind: object_kind.to_owned(),
        object,
        peer_thread,
    })
}

const STRUCTURED_CHOICE_PREFIX: &str = "THES:CHOICE:";

fn campaign_structured_choices(
    run: &Path,
) -> Result<BTreeMap<String, Vec<StructuredChoiceDecision>>, String> {
    Ok(campaign_structured_choices_from_serial(
        &campaign_serial_logs(run)?,
    ))
}

fn campaign_structured_choices_from_serial(
    serial: &BTreeMap<String, Vec<u8>>,
) -> BTreeMap<String, Vec<StructuredChoiceDecision>> {
    serial
        .iter()
        .filter_map(|(service, contents)| {
            let mut ordinal = 0_u64;
            let decisions = String::from_utf8_lossy(contents)
                .lines()
                .filter_map(|line| {
                    let mut decision = parse_structured_choice_line(line)?;
                    decision.ordinal = ordinal;
                    ordinal = ordinal.saturating_add(1);
                    Some(decision)
                })
                .collect::<Vec<_>>();
            (!decisions.is_empty()).then(|| (service.clone(), decisions))
        })
        .collect()
}

fn parse_structured_choice_line(line: &str) -> Option<StructuredChoiceDecision> {
    let mut fields = line
        .trim()
        .strip_prefix(STRUCTURED_CHOICE_PREFIX)?
        .split(':');
    let name = fields.next()?;
    let upper_exclusive = fields.next()?.parse::<u16>().ok()?;
    let selected = fields.next()?.parse::<u16>().ok()?;
    if fields.next().is_some()
        || !valid_coverage_name(name)
        || upper_exclusive == 0
        || upper_exclusive > 256
        || selected >= upper_exclusive
    {
        return None;
    }
    Some(StructuredChoiceDecision {
        ordinal: 0,
        name: name.to_owned(),
        upper_exclusive,
        selected,
    })
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

/// Low-overhead execution coverage captured by vCPU threads at deterministic
/// handled-exit quanta. A legacy checkpoint without this runtime-only state
/// falls back to its paused PC, so old locked bundles remain readable.
fn campaign_checkpoint_execution_locations(
    checkpoint: &CampaignCheckpoint,
) -> BTreeMap<String, Vec<String>> {
    checkpoint
        .scheduler
        .iter()
        .map(|(service, state)| {
            let locations = state
                .execution_locations
                .as_ref()
                .filter(|samples| !samples.is_empty())
                .map(|samples| {
                    samples
                        .iter()
                        .flat_map(|vcpu| vcpu.iter())
                        .map(|location| format!("{location:#x}"))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect()
                })
                .unwrap_or_else(|| {
                    state
                        .program_counters
                        .iter()
                        .map(|location| format!("{location:#x}"))
                        .collect()
                });
            (service.clone(), locations)
        })
        .collect()
}

fn campaign_checkpoint_execution_ledgers(
    checkpoint: &CampaignCheckpoint,
) -> BTreeMap<String, Vec<ExecutionLedgerEvidence>> {
    checkpoint
        .scheduler
        .iter()
        .filter_map(|(service, state)| {
            state.execution_ledgers.as_ref().map(|ledgers| {
                (
                    service.clone(),
                    ledgers.iter().map(ExecutionLedger::evidence).collect(),
                )
            })
        })
        .collect()
}

fn campaign_checkpoint_machine_execution_ledgers(
    checkpoint: &CampaignCheckpoint,
) -> BTreeMap<String, ExecutionLedgerEvidence> {
    checkpoint
        .scheduler
        .iter()
        .filter_map(|(service, state)| {
            state
                .machine_execution_state
                .as_ref()
                .map(|execution| (service.clone(), execution.ledger_evidence()))
        })
        .collect()
}

fn campaign_checkpoint_boundary(
    checkpoint: &CampaignCheckpoint,
    actions: Vec<AppliedCampaignAction>,
) -> CampaignCheckpointBoundary {
    CampaignCheckpointBoundary {
        actions,
        round: checkpoint.round,
        markers: campaign_checkpoint_markers(checkpoint)
            .into_iter()
            .collect(),
        application_blocks: campaign_application_blocks_from_serial(
            &campaign_checkpoint_serial_contents(checkpoint),
        ),
        thread_scheduling: campaign_thread_scheduling_from_serial(
            &campaign_checkpoint_serial_contents(checkpoint),
        ),
        thread_synchronization: campaign_thread_synchronization_from_serial(
            &campaign_checkpoint_serial_contents(checkpoint),
        ),
        structured_choices: campaign_structured_choices_from_serial(
            &campaign_checkpoint_serial_contents(checkpoint),
        ),
        program_counters: campaign_checkpoint_program_counters(checkpoint),
        serial_sha256: campaign_checkpoint_serial_sha256(checkpoint),
        serial_contents: campaign_checkpoint_serial_contents(checkpoint),
        serial_pending_bytes: checkpoint
            .scheduler
            .iter()
            .map(|(service, state)| (service.clone(), state.serial_pending_bytes))
            .collect(),
        network_traffic: checkpoint
            .scheduler
            .iter()
            .map(|(service, state)| (service.clone(), state.network_traffic.clone()))
            .collect(),
        storage_sha256: checkpoint
            .scheduler
            .iter()
            .map(|(service, state)| (service.clone(), state.storage_sha256.clone()))
            .collect(),
        virtual_time_ns: checkpoint
            .scheduler
            .iter()
            .filter_map(|(service, state)| {
                state
                    .virtual_time_ns
                    .as_ref()
                    .map(|time| (service.clone(), time.clone()))
            })
            .collect(),
        execution_ledgers: campaign_checkpoint_execution_ledgers(checkpoint),
        machine_execution_ledgers: campaign_checkpoint_machine_execution_ledgers(checkpoint),
    }
}

fn campaign_checkpoint_serial_contents(
    checkpoint: &CampaignCheckpoint,
) -> BTreeMap<String, Vec<u8>> {
    checkpoint
        .scheduler
        .iter()
        .map(|(service, state)| {
            (
                service.clone(),
                state.serial_contents.iter().flatten().copied().collect(),
            )
        })
        .collect()
}

fn campaign_checkpoint_serial_sha256(checkpoint: &CampaignCheckpoint) -> BTreeMap<String, String> {
    checkpoint
        .scheduler
        .iter()
        .map(|(service, state)| {
            let mut hasher = Sha256::new();
            for serial in &state.serial_contents {
                hasher.update((serial.len() as u64).to_le_bytes());
                hasher.update(serial);
            }
            (service.clone(), format!("{:x}", hasher.finalize()))
        })
        .collect()
}

fn campaign_operation_timeline(
    campaign: &CampaignPlan,
    schedule: &CampaignSchedule,
    events: &[CampaignEvent],
    barriers: &[CampaignUartBarrier],
    boundaries: &[CampaignCheckpointBoundary],
    baseline: &CampaignCheckpointBoundary,
    symbolizer: &CampaignInstructionSymbolizer,
    application_symbolizer: &CampaignApplicationSymbolizer,
) -> Vec<CampaignTimelineBoundary> {
    debug_assert_eq!(schedule.operations.len(), events.len());
    debug_assert_eq!(schedule.operations.len(), barriers.len());
    debug_assert_eq!(schedule.operations.len(), boundaries.len());
    let mut previous = baseline.clone();
    schedule
        .operations
        .iter()
        .zip(events)
        .zip(barriers)
        .zip(boundaries)
        .enumerate()
        .map(|(index, (((operation, event), barrier), boundary))| {
            let (new_markers, changed_program_counters, changed_serial) =
                campaign_boundary_delta(&previous, boundary);
            let previous_application_blocks =
                campaign_application_block_ids(&previous.application_blocks)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
            let new_application_blocks =
                campaign_application_block_ids(&boundary.application_blocks)
                    .into_iter()
                    .filter(|block| !previous_application_blocks.contains(block))
                    .collect();
            let new_thread_scheduling_decisions = campaign_thread_scheduling_delta(
                &previous.thread_scheduling,
                &boundary.thread_scheduling,
            );
            let new_thread_synchronization_events = campaign_thread_synchronization_delta(
                &previous.thread_synchronization,
                &boundary.thread_synchronization,
            );
            let new_structured_choices = campaign_structured_choice_delta(
                &previous.structured_choices,
                &boundary.structured_choices,
            );
            let serial_delta = campaign_serial_delta(&previous, boundary);
            let network_traffic_delta = campaign_network_traffic_delta(&previous, boundary);
            let (changed_storage, virtual_time_delta_ns) =
                campaign_boundary_state_delta(&previous, boundary);
            let input = campaign_input_evidence(&event.event.data_hex);
            let delivery =
                campaign_uart_delivery(&event.service, &event.event, &input, &previous, boundary);
            previous = boundary.clone();
            let mut application_blocks = boundary.application_blocks.clone();
            application_symbolizer.symbolize(&mut application_blocks);
            CampaignTimelineBoundary {
                id: format!(
                    "op-{index:03}-{}",
                    campaign_operation_choice_name(campaign, *operation)
                ),
                operation: campaign_operation_choice_name(campaign, *operation),
                command: campaign.operations[operation.operation].command,
                test_command_path: campaign.operations[operation.operation]
                    .test_command_path
                    .clone(),
                terminated_command_services: event.terminate_shell_processes.clone(),
                service: campaign_operation_service(campaign, *operation).to_owned(),
                input,
                delivery,
                barrier: barrier.clone(),
                round: boundary.round,
                actions: boundary.actions.clone(),
                markers: boundary.markers.clone(),
                new_markers,
                changed_program_counters,
                changed_serial,
                program_counters: boundary.program_counters.clone(),
                instruction_locations: symbolizer.symbolize(&boundary.program_counters),
                application_blocks,
                new_application_blocks,
                thread_scheduling: boundary.thread_scheduling.clone(),
                new_thread_scheduling_decisions,
                thread_synchronization: boundary.thread_synchronization.clone(),
                new_thread_synchronization_events,
                structured_choices: boundary.structured_choices.clone(),
                new_structured_choices,
                serial_sha256: boundary.serial_sha256.clone(),
                serial_delta,
                network_traffic_delta,
                changed_storage,
                virtual_time_delta_ns,
                execution_ledgers: boundary.execution_ledgers.clone(),
                machine_execution_ledgers: boundary.machine_execution_ledgers.clone(),
                state_sha256: campaign_boundary_state_sha256(boundary),
            }
        })
        .collect()
}

fn campaign_thread_scheduling_delta(
    previous: &BTreeMap<String, Vec<ThreadSchedulingDecision>>,
    current: &BTreeMap<String, Vec<ThreadSchedulingDecision>>,
) -> BTreeMap<String, Vec<ThreadSchedulingDecision>> {
    current
        .iter()
        .filter_map(|(service, decisions)| {
            let prefix = previous
                .get(service)
                .filter(|prior| decisions.starts_with(prior))
                .map(Vec::len)
                .unwrap_or(0);
            (prefix < decisions.len()).then(|| (service.clone(), decisions[prefix..].to_vec()))
        })
        .collect()
}

fn campaign_thread_synchronization_delta(
    previous: &BTreeMap<String, Vec<ThreadSynchronizationEvent>>,
    current: &BTreeMap<String, Vec<ThreadSynchronizationEvent>>,
) -> BTreeMap<String, Vec<ThreadSynchronizationEvent>> {
    current
        .iter()
        .filter_map(|(service, events)| {
            let prefix = previous
                .get(service)
                .filter(|prior| events.starts_with(prior))
                .map(Vec::len)
                .unwrap_or(0);
            (prefix < events.len()).then(|| (service.clone(), events[prefix..].to_vec()))
        })
        .collect()
}

fn campaign_structured_choice_delta(
    previous: &BTreeMap<String, Vec<StructuredChoiceDecision>>,
    current: &BTreeMap<String, Vec<StructuredChoiceDecision>>,
) -> BTreeMap<String, Vec<StructuredChoiceDecision>> {
    current
        .iter()
        .filter_map(|(service, decisions)| {
            let prefix = previous
                .get(service)
                .filter(|prior| decisions.starts_with(prior))
                .map(Vec::len)
                .unwrap_or(0);
            (prefix < decisions.len()).then(|| (service.clone(), decisions[prefix..].to_vec()))
        })
        .collect()
}

fn campaign_uart_delivery(
    service: &str,
    event: &EventPlan,
    input: &CampaignInputEvidence,
    previous: &CampaignCheckpointBoundary,
    boundary: &CampaignCheckpointBoundary,
) -> CampaignUartDelivery {
    let pending_before = previous
        .serial_pending_bytes
        .get(service)
        .copied()
        .unwrap_or_default();
    let pending_after = boundary
        .serial_pending_bytes
        .get(service)
        .copied()
        .unwrap_or_default();
    CampaignUartDelivery {
        recorded: true,
        accepted_bytes: input.bytes,
        pending_before,
        pending_after,
        // The runner is the only UART input producer during a campaign. A
        // successful raw_input call accepts all bytes, so this is the exact
        // FIFO dequeue count over the operation window.
        guest_read_bytes: pending_before
            .saturating_add(input.bytes)
            .saturating_sub(pending_after),
        checkpoint: event.checkpoint.clone().unwrap_or_default(),
    }
}

fn campaign_boundary_state_delta(
    previous: &CampaignCheckpointBoundary,
    boundary: &CampaignCheckpointBoundary,
) -> (Vec<String>, BTreeMap<String, Vec<u64>>) {
    let changed_storage = boundary
        .storage_sha256
        .iter()
        .flat_map(|(service, drives)| {
            drives.iter().filter_map(move |(drive, hash)| {
                (previous
                    .storage_sha256
                    .get(service)
                    .and_then(|previous| previous.get(drive))
                    != Some(hash))
                .then(|| format!("{service}:{drive}"))
            })
        })
        .collect();
    let virtual_time_delta_ns = boundary
        .virtual_time_ns
        .iter()
        .filter_map(|(service, current)| {
            let previous = previous.virtual_time_ns.get(service);
            let delta = current
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    value.saturating_sub(
                        previous
                            .and_then(|time| time.get(index))
                            .copied()
                            .unwrap_or(0),
                    )
                })
                .collect::<Vec<_>>();
            delta
                .iter()
                .any(|value| *value > 0)
                .then(|| (service.clone(), delta))
        })
        .collect();
    (changed_storage, virtual_time_delta_ns)
}

/// The compact boundary identity covers all checkpoint evidence that feeds the
/// operation report. It makes an individual row independently comparable
/// across replay results without changing campaign scheduling.
fn campaign_boundary_state_sha256(boundary: &CampaignCheckpointBoundary) -> String {
    let encoded = serde_json::to_vec(&(
        &boundary.markers,
        boundary.round,
        &boundary.program_counters,
        &boundary.serial_sha256,
        &boundary.serial_pending_bytes,
        &boundary.network_traffic,
        &boundary.storage_sha256,
        &boundary.virtual_time_ns,
        &boundary.execution_ledgers,
    ))
    .expect("campaign boundary state encodes");
    format!("{:x}", Sha256::digest(encoded))
}

fn campaign_network_traffic_delta(
    previous: &CampaignCheckpointBoundary,
    boundary: &CampaignCheckpointBoundary,
) -> BTreeMap<String, BTreeMap<String, CampaignNetworkTrafficDelta>> {
    boundary
        .network_traffic
        .iter()
        .filter_map(|(service, networks)| {
            let previous_networks = previous.network_traffic.get(service);
            let delta = networks
                .iter()
                .filter_map(|(network, current)| {
                    let previous = previous_networks
                        .and_then(|networks| networks.get(network))
                        .cloned()
                        .unwrap_or_default();
                    let delta = CampaignNetworkTrafficDelta {
                        tx_frames: current.tx_frames.saturating_sub(previous.tx_frames),
                        rx_frames: current.rx_frames.saturating_sub(previous.rx_frames),
                        dropped: current.dropped.saturating_sub(previous.dropped),
                        duplicated: current.duplicated.saturating_sub(previous.duplicated),
                        corrupted: current.corrupted.saturating_sub(previous.corrupted),
                    };
                    (delta
                        != CampaignNetworkTrafficDelta {
                            tx_frames: 0,
                            rx_frames: 0,
                            dropped: 0,
                            duplicated: 0,
                            corrupted: 0,
                        })
                    .then_some((network.clone(), delta))
                })
                .collect::<BTreeMap<_, _>>();
            (!delta.is_empty()).then_some((service.clone(), delta))
        })
        .collect()
}

const CAMPAIGN_EVIDENCE_EXCERPT_BYTES: usize = 512;

fn campaign_serial_delta(
    previous: &CampaignCheckpointBoundary,
    boundary: &CampaignCheckpointBoundary,
) -> BTreeMap<String, CampaignSerialDelta> {
    boundary
        .serial_contents
        .iter()
        .filter_map(|(service, contents)| {
            let previous_contents = previous
                .serial_contents
                .get(service)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let delta = contents
                .strip_prefix(previous_contents)
                .unwrap_or(contents.as_slice());
            (!delta.is_empty()).then(|| (service.clone(), campaign_serial_evidence(delta)))
        })
        .collect()
}

fn campaign_serial_evidence(bytes: &[u8]) -> CampaignSerialDelta {
    let excerpt = &bytes[..bytes.len().min(CAMPAIGN_EVIDENCE_EXCERPT_BYTES)];
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    CampaignSerialDelta {
        bytes: bytes.len(),
        sha256: format!("{:x}", hasher.finalize()),
        excerpt: excerpt
            .iter()
            .flat_map(|byte| std::ascii::escape_default(*byte))
            .map(char::from)
            .collect(),
        omitted_bytes: bytes.len() - excerpt.len(),
    }
}

/// Compact, deterministic evidence of what an operation changed relative to
/// its immediately preceding checkpoint.
fn campaign_boundary_delta(
    previous: &CampaignCheckpointBoundary,
    boundary: &CampaignCheckpointBoundary,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let previous_markers = previous.markers.iter().collect::<BTreeSet<_>>();
    let new_markers = boundary
        .markers
        .iter()
        .filter(|marker| !previous_markers.contains(marker))
        .cloned()
        .collect();
    let changed_program_counters = boundary
        .program_counters
        .iter()
        .filter_map(|(service, counters)| {
            (previous.program_counters.get(service) != Some(counters)).then(|| service.clone())
        })
        .collect();
    let changed_serial = boundary
        .serial_sha256
        .iter()
        .filter_map(|(service, hash)| {
            (previous.serial_sha256.get(service) != Some(hash)).then(|| service.clone())
        })
        .collect();
    (new_markers, changed_program_counters, changed_serial)
}

/// A paused vCPU PC is a deterministic instruction-location sample. The
/// service name keeps identical guest addresses in distinct VM images apart.
fn campaign_instruction_locations(program_counters: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    program_counters
        .iter()
        .flat_map(|(service, counters)| {
            counters
                .iter()
                .map(move |counter| format!("{service}:{counter}"))
        })
        .collect()
}

#[derive(Debug, Clone)]
struct KernelSymbol {
    address: u64,
    size: u64,
    name: String,
}

/// Resolve checkpoint PCs from the bundle-local kernel copies once per
/// campaign. Symbolization never affects scheduling or replay: a kernel can
/// be stripped, non-ELF, or have no matching symbol and still retains its raw
/// deterministic address in the report.
struct CampaignInstructionSymbolizer {
    symbols: BTreeMap<String, Vec<KernelSymbol>>,
    sources: BTreeMap<String, Loader>,
}

/// Join build-scoped userspace coverage records with the exact symbol files
/// locked beside the campaign. The key includes the service because two
/// guests may run the same process/module build independently.
struct CampaignApplicationSymbolizer {
    entries: BTreeMap<(String, String, String, String), (Vec<KernelSymbol>, Loader)>,
}

impl CampaignApplicationSymbolizer {
    fn from_topology(topology: &TopologyPlan) -> Self {
        let mut entries = BTreeMap::new();
        for (service, plan) in &topology.services {
            for coverage in &plan.coverage {
                let path = Path::new(&coverage.symbols.path);
                if let Ok(loader) = Loader::new(path) {
                    entries.insert(
                        (
                            service.clone(),
                            coverage.process.clone(),
                            coverage.module.clone(),
                            coverage.build_sha256.clone(),
                        ),
                        (kernel_symbols(path), loader),
                    );
                }
            }
        }
        Self { entries }
    }

    fn symbolize(&self, coverage: &mut BTreeMap<String, Vec<ApplicationBlock>>) {
        for (service, points) in coverage {
            for point in points {
                let key = (
                    service.clone(),
                    point.process.clone(),
                    point.module.clone(),
                    point.build_sha256.clone(),
                );
                let Some((symbols, sources)) = self.entries.get(&key) else {
                    continue;
                };
                let location =
                    symbolize_instruction_location(&point.offset, symbols, Some(sources));
                point.symbol = location.symbol;
                point.symbol_offset = location.offset;
                point.source = location.source;
            }
        }
    }
}

impl CampaignInstructionSymbolizer {
    fn from_topology(topology: &TopologyPlan) -> Self {
        let mut symbols = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for (service, plan) in &topology.services {
            let kernel = Path::new(&plan.run.guest.kernel.path);
            let kernel_symbols = kernel_symbols(kernel);
            if !kernel_symbols.is_empty() {
                symbols.insert(service.clone(), kernel_symbols);
            }
            if let Ok(source_locations) = Loader::new(kernel) {
                sources.insert(service.clone(), source_locations);
            }
        }
        Self { symbols, sources }
    }

    fn symbolize(
        &self,
        program_counters: &BTreeMap<String, Vec<String>>,
    ) -> BTreeMap<String, Vec<InstructionLocation>> {
        program_counters
            .iter()
            .map(|(service, counters)| {
                let symbols = self.symbols.get(service).map(Vec::as_slice).unwrap_or(&[]);
                let sources = self.sources.get(service);
                (
                    service.clone(),
                    counters
                        .iter()
                        .map(|address| symbolize_instruction_location(address, symbols, sources))
                        .collect(),
                )
            })
            .collect()
    }
}

fn kernel_symbols(path: &Path) -> Vec<KernelSymbol> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(file) = object::File::parse(&*bytes) else {
        return Vec::new();
    };
    let mut symbols = file
        .symbols()
        .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.address() != 0)
        .filter_map(|symbol| {
            symbol.name().ok().map(|name| KernelSymbol {
                address: symbol.address(),
                size: symbol.size(),
                name: name.to_owned(),
            })
        })
        .collect::<Vec<_>>();
    symbols.sort_by(|left, right| {
        left.address
            .cmp(&right.address)
            .then_with(|| left.name.cmp(&right.name))
    });
    symbols.dedup_by(|left, right| left.address == right.address && left.name == right.name);
    symbols
}

fn symbolize_instruction_location(
    address: &str,
    symbols: &[KernelSymbol],
    sources: Option<&Loader>,
) -> InstructionLocation {
    let parsed = address
        .strip_prefix("0x")
        .and_then(|address| u64::from_str_radix(address, 16).ok());
    let symbol = parsed.and_then(|address| {
        let index = symbols.partition_point(|symbol| symbol.address <= address);
        index.checked_sub(1).and_then(|index| {
            let symbol = &symbols[index];
            let end = if symbol.size == 0 {
                symbols
                    .get(index + 1)
                    .map(|symbol| symbol.address)
                    .unwrap_or(u64::MAX)
            } else {
                symbol.address.saturating_add(symbol.size)
            };
            (address < end).then(|| (symbol.name.clone(), address.saturating_sub(symbol.address)))
        })
    });
    InstructionLocation {
        address: address.to_owned(),
        symbol: symbol.as_ref().map(|(name, _)| name.clone()),
        offset: symbol.map(|(_, offset)| offset),
        source: parsed.and_then(|address| {
            sources
                .and_then(|sources| sources.find_location(address).ok().flatten())
                .and_then(|location| {
                    Some(InstructionSourceLocation {
                        file: report_source_path(location.file?),
                        line: location.line?,
                        column: location.column,
                    })
                })
        }),
    }
}

/// DWARF commonly records an absolute build directory. A report must not leak
/// it, so preserve relative paths and reduce absolute paths to a stable file
/// name. The locked kernel image remains the authoritative source artifact.
fn report_source_path(path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown-source")
            .to_owned()
    } else {
        path.to_string_lossy().into_owned()
    }
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
                PropertyKind::AlwaysOrUnreachable => {
                    found == runs.len() || found == 0
                }
                PropertyKind::Sometimes | PropertyKind::Reachable => found > 0,
                PropertyKind::Unreachable => found == 0,
            };
            let kind = match property.kind {
                PropertyKind::Always => "always",
                PropertyKind::AlwaysOrUnreachable => "always_or_unreachable",
                PropertyKind::Sometimes => "sometimes",
                PropertyKind::Reachable => "reachable",
                PropertyKind::Unreachable => "unreachable",
            };
            let mut detail = format!(
                "{} of {} retained timelines satisfied {}",
                found,
                runs.len(),
                campaign_property_description(property)
            );
            if !passed {
                let failing = property_first_failing_run(property.kind, &matches)
                    .map(|index| runs[index].index)
                    .unwrap_or_default();
                if let Some(context) =
                    property_violation_context(property, &output.join("runs").join(format!("{failing:03}")))
                {
                    detail.push_str(&format!(" — first violation: {context}"));
                }
            }
            Ok(CampaignPropertyResult {
                name: property.name.clone(),
                kind,
                status: if passed { "passed" } else { "failed" },
                detail,
            })
        })
        .collect()
}

/// The primary serial needle a property observes, for violation context.
fn property_violation_needle(property: &CampaignProperty) -> Option<String> {
    if let Some(contains) = property.contains.as_deref() {
        return Some(contains.to_owned());
    }
    property
        .contains_all
        .first()
        .or_else(|| property.contains_any.first())
        .or_else(|| property.contains_none.first())
        .cloned()
        .or_else(|| {
            property
                .predicate
                .as_ref()
                .and_then(|predicate| predicate.contains.clone())
        })
}

/// A bounded excerpt of the serial log around the first needle occurrence,
/// or the log tail when the needle never appeared.
fn serial_violation_excerpt(serial: &[u8], needle: Option<&str>) -> String {
    const MAX: usize = 240;
    let lossy = String::from_utf8_lossy(serial);
    let offset = needle.and_then(|needle| lossy.find(needle));
    let (start, end) = match offset {
        Some(at) => {
            let before = lossy[..at].rfind('\n').map(|at| at + 1).unwrap_or(0);
            let before = lossy[..before]
                .rfind('\n')
                .map(|at| at + 1)
                .unwrap_or(before);
            let after = lossy[at..]
                .find('\n')
                .map(|line_end| at + line_end + 1)
                .unwrap_or(lossy.len());
            let after = lossy[after..]
                .find('\n')
                .map(|line_end| after + line_end + 1)
                .unwrap_or(lossy.len());
            (before, after)
        }
        None => (lossy.len().saturating_sub(MAX), lossy.len()),
    };
    let mut excerpt: String = lossy[start..end.min(lossy.len())].chars().collect();
    if excerpt.len() > MAX {
        excerpt = excerpt
            .chars()
            .skip(excerpt.len() - MAX)
            .collect::<String>();
    }
    let trimmed = excerpt.trim();
    let mut compact = trimmed.replace('\r', "");
    if start > 0 {
        compact.insert_str(0, "…");
    }
    if end < lossy.len() {
        compact.push('…');
    }
    compact
}

/// Locate a failed property's first violation in one retained timeline: the
/// service and bounded serial excerpt around the primary needle, or the log
/// tail when the needle never appeared.
fn property_violation_context(property: &CampaignProperty, run: &Path) -> Option<String> {
    let needle = property_violation_needle(property);
    let services = campaign_property_services(run, property.service.as_deref());
    let mut fallback: Option<String> = None;
    for service in services {
        let serial = campaign_serial_contents(run, &service);
        let hit = needle
            .as_deref()
            .is_some_and(|needle| serial_contains(&serial, needle));
        let excerpt = serial_violation_excerpt(&serial, needle.as_deref());
        let context = format!("service {service}: {excerpt}");
        if hit {
            return Some(context);
        }
        if fallback.is_none() {
            fallback = Some(context);
        }
    }
    fallback
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

fn serial_workflow_matches_property(run: &Path, workflow: &SerialWorkflow) -> bool {
    serial_workflow_matches(
        workflow,
        workflow
            .stages
            .iter()
            .map(|stage| {
                serial_workflow_stage_values(
                    &campaign_serial_contents(run, &stage.service),
                    workflow,
                    stage,
                )
            })
            .collect(),
    )
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
    } else if let Some(workflow) = &evidence.workflow {
        serial_workflow_matches_property(run, workflow)
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

fn serial_sequence_capture_values(
    serial: &[u8],
    sequence: &[SerialPredicate],
    pointer: &str,
) -> Vec<serde_json::Value> {
    serial_sequence_capture_values_from(serial, sequence, 0, 0, BTreeMap::new(), pointer)
}

fn serial_sequence_capture_values_from(
    serial: &[u8],
    sequence: &[SerialPredicate],
    index: usize,
    offset: usize,
    captures: BTreeMap<String, serde_json::Value>,
    pointer: &str,
) -> Vec<serde_json::Value> {
    let Some(predicate) = sequence.get(index) else {
        return Vec::new();
    };
    if index + 1 == sequence.len() {
        let Some(predicate) = &predicate.json else {
            return Vec::new();
        };
        return serial_json_predicate_capture_values(
            &serial[offset..],
            predicate,
            &captures,
            pointer,
        );
    }
    serial_sequence_item_match_ends(&serial[offset..], predicate, &captures)
        .into_iter()
        .flat_map(|(end, captures)| {
            serial_sequence_capture_values_from(
                serial,
                sequence,
                index + 1,
                offset + end,
                captures,
                pointer,
            )
        })
        .collect()
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

fn serial_json_predicate_capture_values(
    serial: &[u8],
    predicate: &JsonPredicate,
    captures: &BTreeMap<String, serde_json::Value>,
    pointer: &str,
) -> Vec<serde_json::Value> {
    serial
        .split_inclusive(|byte| *byte == b'\n')
        .filter_map(|line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let event = serde_json::from_slice::<serde_json::Value>(line).ok()?;
            let mut captures = captures.clone();
            json_predicate_matches_with_captures(&event, predicate, &mut captures)
                .then(|| event.pointer(pointer).cloned())
                .flatten()
        })
        .collect()
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
    predicate.query.as_ref().is_none_or(|query| {
        JsonPath::parse(query)
            .map(|query| !query.query(event).all().is_empty())
            .unwrap_or(false)
    }) && predicate
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
            .all
            .iter()
            .all(|predicate| json_predicate_matches(event, predicate))
        && (predicate.any.is_empty()
            || predicate
                .any
                .iter()
                .any(|predicate| json_predicate_matches(event, predicate)))
        && predicate
            .none
            .iter()
            .all(|predicate| !json_predicate_matches(event, predicate))
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
    } else if let Some(workflow) = &evidence.workflow {
        serial_workflow_description(workflow)
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

fn serial_workflow_description(workflow: &SerialWorkflow) -> String {
    let stages = workflow
        .stages
        .iter()
        .map(|stage| {
            let pointers = if stage.pointers.is_empty() {
                workflow.pointers.join(" + ")
            } else {
                stage.pointers.join(" + ")
            };
            format!(
                "{} ({} steps; {pointers})",
                stage.service,
                stage.steps.len()
            )
        })
        .collect::<Vec<_>>()
        .join(" -> ");
    let description = format!(
        "workflow [{stages}] keyed by {}",
        workflow.pointers.join(" + ")
    );
    let description = match workflow.quantifier {
        SerialJoinQuantifier::Any => description,
        SerialJoinQuantifier::Every => format!("every {description}"),
    };
    workflow
        .occurs
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
        if !json.fields.is_empty() {
            clauses.push(format!("JSON event has fields {:?}", json.fields));
        }
        if let Some(query) = &json.query {
            clauses.push(format!("JSONPath {query:?} selects a node"));
        }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutionCompletion {
    GuestExit,
    CampaignCheckpoint,
}

fn execute(
    mut topology: TopologyPlan,
    output: &Path,
    checkpoint: Option<&CampaignCheckpoint>,
    completion: ExecutionCompletion,
    expected_serial: Option<BTreeMap<String, Vec<String>>>,
    expected_faults: Option<BTreeMap<String, String>>,
    expected_network: Option<String>,
    expected_actions: Option<Vec<AppliedCampaignAction>>,
    expected_storage: Option<BTreeMap<String, BTreeMap<String, String>>>,
    expected_traffic: Option<BTreeMap<String, BTreeMap<String, NetworkTraffic>>>,
    expected_entropy: Option<BTreeMap<String, String>>,
    expected_virtual_time: Option<BTreeMap<String, Option<Vec<u64>>>>,
    expected_execution_ledgers: Option<BTreeMap<String, Vec<ExecutionLedgerEvidence>>>,
    expected_machine_execution_ledgers: Option<BTreeMap<String, ExecutionLedgerEvidence>>,
    expected_machine_execution_traces: Option<BTreeMap<String, Vec<String>>>,
    expected_lifecycle_rounds: Option<u64>,
) -> Result<(), String> {
    // An exported campaign is a fixed-schedule plan, but its terminal state is
    // still the final operation checkpoint. Its service entrypoints are often
    // daemons and are not expected to exit.
    let completion = if topology.machine_replay == MachineReplayMode::HostInputs {
        ExecutionCompletion::CampaignCheckpoint
    } else {
        completion
    };
    let control_replay = completion == ExecutionCompletion::CampaignCheckpoint
        || topology.machine_replay == MachineReplayMode::HostInputs;
    // Checkpoint-backed campaign leaves skip the artifact-locking branch
    // below, but they still need an output root before the replay plan can be
    // made relative to it.  Create the root for both fresh and restored runs.
    fs::create_dir_all(output).map_err(|error| {
        format!(
            "cannot create topology output directory {}: {error}",
            output.display()
        )
    })?;
    configure_container_networks(&mut topology)?;
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
            let service_dir = output.join("services").join(name);
            fs::create_dir_all(service_dir.join("artifacts")).map_err(|error| error.to_string())?;
            let locked = topology
                .services
                .get_mut(name)
                .expect("topology service missing");
            lock_service_inputs(&service_dir, locked)?;
        }
    }
    write_replay_plan(&output.join("replay-plan.json"), &topology)?;
    for name in &names {
        let service = &topology.services[name];
        let service_dir = output.join("services").join(name);
        fs::create_dir_all(&service_dir).map_err(|error| error.to_string())?;
        let serial = service_dir.join("serial.log");
        let (vm, serial_logs, next_fault, paused_until, throttle, faults, network_traffic, network_trace) =
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
                    service_initramfs(service)?,
                    serial,
                    &switches,
                    scheduler.execution_locations.as_deref(),
                    scheduler.execution_ledgers.as_deref(),
                    scheduler.machine_execution_state.as_ref(),
                    scheduler.devices.as_ref(),
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
                    scheduler.throttle.clone(),
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
                        service_initramfs(service)?,
                        &serial,
                        &mut switches,
                    )?,
                    vec![serial],
                    0,
                    None,
                    None,
                    Vec::new(),
                    BTreeMap::new(),
                    BTreeMap::new(),
                )
            };
        if let Some(expected) = &expected_machine_execution_traces {
            let trace = expected
                .get(name)
                .ok_or_else(|| format!("recorded machine execution trace missing service {name}"))?
                .clone();
            if control_replay {
                vm.enforce_machine_execution_control_trace(trace)?;
            } else {
                vm.enforce_machine_execution_trace(trace)?;
            }
        }
        services.insert(
            name.clone(),
            ServiceRuntime {
                vm,
                serial_logs,
                next_fault,
                paused_until,
                throttle,
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
    let max_rounds = topology
        .services
        .values()
        .map(|service| service.run.run.max_rounds)
        .max()
        .unwrap_or_else(default_max_rounds);
    let mut round = checkpoint.map_or(0, |checkpoint| checkpoint.round);
    if checkpoint.is_some() {
        for name in &names {
            services[name].vm.resume()?;
        }
    } else {
        round = round.saturating_add(start_services_in_dependency_order(
            &topology,
            &mut services,
            &switches,
            max_rounds.saturating_sub(round),
        )?);
    }
    let mut actions = Vec::new();
    let mut lifecycle_barrier_rounds: u64 = 0;
    let ordered_events = ordered_topology_events(&topology)?;
    let mut ready_services = BTreeSet::new();
    for (name, event) in ordered_events {
        if ready_services.insert(name.clone()) {
            let serial = services[&name].serial_logs[0].clone();
            let remaining = max_rounds.saturating_sub(round);
            round = round.saturating_add(wait_for_serial_with_topology_rounds(
                &serial,
                b"THES:M:42",
                "serial readiness",
                &mut services,
                &switches,
                remaining,
            )?);
        }
        let mut driver = services.remove(&name).expect("topology service missing");
        let serial = driver.serial_logs[0].clone();
        inject_campaign_events(
            &name,
            &mut driver,
            std::slice::from_ref(&event),
            &serial,
            &topology,
            &mut services,
            &switches,
            &mut round,
            &mut actions,
        )?;
        services.insert(name, driver);
    }
    // A campaign branch ends at its last operation checkpoint. Container
    // entrypoints are usually daemons, so waiting for guest exit would only
    // burn the complete round budget and incorrectly fail a valid branch.
    while completion == ExecutionCompletion::GuestExit
        && round.saturating_add(lifecycle_barrier_rounds) < max_rounds
        && services
            .values()
            .any(|service| service.vm.exited().is_none())
    {
        round += 1;
        for name in topology.services.keys() {
            let mut service = services.remove(name).expect("topology service missing");
            let remaining_rounds =
                max_rounds.saturating_sub(round.saturating_add(lifecycle_barrier_rounds));
            lifecycle_barrier_rounds =
                lifecycle_barrier_rounds.saturating_add(apply_scheduled_faults(
                    round,
                    name,
                    &topology.services[name],
                    &output.join("services").join(name),
                    &mut service,
                    &mut services,
                    &mut switches,
                    remaining_rounds,
                )?);
            let throttled = service.throttle.as_ref().is_some_and(|state| {
                state.until_round > round && round % u64::from(state.every_n_rounds) != 0
            });
            if service.paused_until.is_none()
                && !throttled
                && service.vm.exited().is_none()
            {
                service.vm.pump();
            }
            services.insert(name.clone(), service);
        }
        advance_network_round(&switches, &services)?;
    }
    if let Some(expected) = &expected_machine_execution_traces {
        complete_machine_execution_replay(&mut services, expected, max_rounds, control_replay)?;
    }
    // A campaign ends at an operation checkpoint, not at guest exit. Freeze
    // every still-running vCPU before reading the terminal evidence so the
    // per-vCPU ledgers and complete machine trace describe one exact cut.
    // Pause is idempotent for services already held by a scheduled fault.
    for service in services.values() {
        if service.vm.exited().is_none() {
            if let Err(error) = service.vm.pause() {
                // A short-lived guest can exit between the state check and
                // the pause request. Its terminal state is already stable.
                if service.vm.exited().is_none() {
                    return Err(error);
                }
            }
        }
    }
    let network_sha256 = network_fingerprint(&switches)?;
    fs::write(
        output.join("topology-result.json"),
        serde_json::to_vec_pretty(&TopologyResult {
            network_sha256: network_sha256.clone(),
            rounds: round,
            max_rounds,
            lifecycle_barrier_rounds,
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
        let (exit_status, mut error, exit_detail) = match exit {
            Some(FcExitCode::Ok) => ("passed", None, "guest exited with status 0".to_owned()),
            Some(code) => {
                let detail = format!("guest exited with {code:?}");
                ("failed", Some(detail.clone()), detail)
            }
            None if completion == ExecutionCompletion::CampaignCheckpoint => (
                "passed",
                None,
                "campaign operation checkpoint reached".to_owned(),
            ),
            None => {
                let detail =
                    "guest did not exit before the configured topology round budget".to_owned();
                ("failed", Some(detail.clone()), detail)
            }
        };
        let mut checks = evaluate_checks(&topology.services[name].run.checks, &service.serial_logs);
        let serial_sha256 = serial_fingerprints(&service.serial_logs)?;
        let faults_sha256 = fault_fingerprint(&service.faults)?;
        let storage_sha256 = service
            .vm
            .storage_fingerprints(&topology.services[name].run.storage)?;
        let entropy_probe_sha256 = service.vm.entropy_probe_sha256();
        let virtual_time_ns = service.vm.virtual_time_ns()?;
        let execution_ledgers = service.vm.execution_ledger_evidence()?;
        let machine_execution_ledger = service.vm.machine_execution_ledger_evidence()?;
        let machine_execution_trace = service.vm.machine_execution_trace()?;
        let machine_execution_replay_error = service.vm.machine_execution_replay_error()?;
        checks.insert(
            0,
            CheckResult {
                name: match completion {
                    ExecutionCompletion::GuestExit => "guest_exit",
                    ExecutionCompletion::CampaignCheckpoint => "campaign_checkpoint",
                }
                .to_owned(),
                status: exit_status,
                detail: exit_detail,
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
        if name == names.first().expect("topology service list is not empty") {
            if let Some(expected) = expected_lifecycle_rounds {
                let matches = expected == lifecycle_barrier_rounds;
                checks.push(CheckResult {
                    name: "replay_lifecycle_rounds".to_owned(),
                    status: if matches { "passed" } else { "failed" },
                    detail: if matches {
                        "lifecycle barrier rounds match the original replay bundle".to_owned()
                    } else {
                        "lifecycle barrier rounds differ from the original replay bundle".to_owned()
                    },
                });
                if !matches && error.is_none() {
                    error = Some("lifecycle round replay evidence changed".to_owned());
                }
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
        if let Some(expected) = &expected_entropy {
            let expected = expected
                .get(name)
                .expect("recorded entropy probe missing service");
            let matches = expected == &entropy_probe_sha256;
            checks.push(CheckResult {
                name: "replay_entropy".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "seeded entropy stream matches the original replay bundle".to_owned()
                } else {
                    "seeded entropy stream differs from the original replay bundle".to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some("entropy replay fingerprint changed".to_owned());
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
        if let Some(expected) = &expected_execution_ledgers {
            let expected = expected
                .get(name)
                .expect("recorded execution ledgers missing service");
            let matches = expected == &execution_ledgers;
            checks.push(CheckResult {
                name: "replay_execution_ledger".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "ordered KVM execution ledger matches the original replay bundle".to_owned()
                } else {
                    "ordered KVM execution ledger differs from the original replay bundle"
                        .to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some(
                    machine_execution_replay_error
                        .clone()
                        .unwrap_or_else(|| "ordered KVM execution replay diverged".to_owned()),
                );
            }
        }
        if let Some(expected) = &expected_machine_execution_ledgers {
            let expected = expected
                .get(name)
                .expect("recorded machine execution ledger missing service");
            let matches = expected == &machine_execution_ledger;
            checks.push(CheckResult {
                name: "replay_machine_execution_ledger".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    "machine-wide execution stream matches the original replay bundle".to_owned()
                } else {
                    "machine-wide execution stream differs from the original replay bundle"
                        .to_owned()
                },
            });
            if !matches && error.is_none() {
                error = Some(
                    machine_execution_replay_error
                        .clone()
                        .unwrap_or_else(|| "machine-wide execution replay diverged".to_owned()),
                );
            }
        }
        if let Some(expected) = &expected_machine_execution_traces {
            let expected = expected
                .get(name)
                .expect("recorded machine execution trace missing service");
            let matches = machine_execution_replay_error.is_none()
                && if control_replay {
                    machine_replay_control_trace(expected)
                        == machine_replay_control_trace(&machine_execution_trace)
                } else {
                    expected == &machine_execution_trace
                };
            checks.push(CheckResult {
                name: "replay_machine_execution_trace".to_owned(),
                status: if matches { "passed" } else { "failed" },
                detail: if matches {
                    if control_replay {
                        "the recorded host-input stream actively governed replay".to_owned()
                    } else {
                        "the recorded machine execution trace actively governed replay".to_owned()
                    }
                } else {
                    machine_execution_replay_error.clone().unwrap_or_else(|| {
                        "machine execution trace differs from the original replay bundle".to_owned()
                    })
                },
            });
            if !matches && error.is_none() {
                error = Some(
                    machine_execution_replay_error
                        .clone()
                        .unwrap_or_else(|| "active machine execution replay diverged".to_owned()),
                );
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
            execution_start: starting_state::origin(&topology, checkpoint, name),
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
            entropy_probe_sha256,
            virtual_time_ns,
            execution_ledgers,
            machine_execution_ledger,
            machine_execution_trace,
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

fn recorded_lifecycle_barrier_rounds(plan: &Path) -> Result<Option<u64>, String> {
    if plan.file_name().and_then(|name| name.to_str()) != Some("replay-plan.json") {
        return Ok(None);
    }
    let result_path = plan
        .parent()
        .ok_or_else(|| format!("replay plan has no parent directory: {}", plan.display()))?
        .join("topology-result.json");
    match fs::read(&result_path) {
        Ok(result) => serde_json::from_slice::<TopologyResult>(&result)
            .map(|result| Some(result.lifecycle_barrier_rounds))
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

fn recorded_entropy_probes(
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
        let Some(probe) = recorded.entropy_probe_sha256 else {
            return Ok(None);
        };
        expected.insert(name.clone(), probe);
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

fn recorded_execution_ledgers(
    plan: &Path,
    services: &[String],
) -> Result<Option<BTreeMap<String, Vec<ExecutionLedgerEvidence>>>, String> {
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
        let Some(ledgers) = recorded.execution_ledgers else {
            return Ok(None);
        };
        expected.insert(name.clone(), ledgers);
    }
    Ok(Some(expected))
}

fn recorded_machine_execution_ledgers(
    plan: &Path,
    services: &[String],
) -> Result<Option<BTreeMap<String, ExecutionLedgerEvidence>>, String> {
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
        let Some(ledger) = recorded.machine_execution_ledger else {
            return Ok(None);
        };
        expected.insert(name.clone(), ledger);
    }
    Ok(Some(expected))
}

fn recorded_machine_execution_traces(
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
        let Some(trace) = recorded.machine_execution_trace else {
            return Ok(None);
        };
        expected.insert(name.clone(), trace);
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
    reject_active_replay_divergence(services)?;
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

/// Preserve the first active divergence even when no guest readiness marker
/// has been emitted. An unstarted replay must not hide behind a round timeout.
fn reject_active_replay_divergence(
    services: &BTreeMap<String, ServiceRuntime>,
) -> Result<(), String> {
    for (name, service) in services {
        if let Some(error) = service.vm.machine_execution_replay_divergence()? {
            for paused in services.values() {
                let _ = paused.vm.pause();
            }
            let path = service
                .serial_logs
                .first()
                .and_then(|serial| serial.parent())
                .ok_or("diverging service has no diagnostic directory")?
                .join("execution-error.json");
            let diagnostic = serde_json::json!({
                "format": "theseus-topology-execution-error-v1", "boundary": "runtime_error",
                "service": name, "replay_error": error,
                "machine": service.vm.machine_execution_ledger_evidence()?,
                "trace": service.vm.machine_execution_trace()?,
                "vcpus": service.vm.execution_ledger_evidence()?,
            });
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path);
            let retained = match file {
                Ok(mut file) => {
                    use std::io::Write;
                    file.write_all(
                        &serde_json::to_vec_pretty(&diagnostic)
                            .map_err(|error| error.to_string())?,
                    )
                    .and_then(|()| file.sync_all())
                    .map_err(|error| error.to_string())
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) => Err(format!("cannot retain {}: {error}", path.display())),
            };
            for stopped in services.values() {
                stopped.vm.stop();
            }
            retained?;
            return Err(format!(
                "service {name:?} active machine replay diverged: {error}; inspect {}",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Reach the retained replay cut before collecting a checkpoint-backed result.
/// Campaigns use the portable host-input control projection; fixed runs
/// require the complete machine trace.
fn complete_machine_execution_replay(
    services: &mut BTreeMap<String, ServiceRuntime>,
    expected: &BTreeMap<String, Vec<String>>,
    max_polls: u64,
    control_only: bool,
) -> Result<(), String> {
    for poll in 0..=max_polls {
        reject_active_replay_divergence(services)?;
        let mut positions = BTreeMap::new();
        let mut incomplete = false;
        for (name, service) in services.iter() {
            let trace = service.vm.machine_execution_trace()?;
            let position = service.vm.machine_execution_replay_position()?;
            let actual_decisions = machine_replay_decisions(&trace, control_only);
            let expected_decisions = machine_replay_decisions(
                expected.get(name).ok_or_else(|| {
                    format!("recorded machine execution trace missing service {name}")
                })?,
                control_only,
            );
            if actual_decisions > expected_decisions {
                return Err(format!(
                    "service {name:?} extended its machine replay past decision {expected_decisions}"
                ));
            }
            incomplete |= actual_decisions < expected_decisions;
            positions.insert(name.clone(), position);
        }
        if !incomplete {
            return Ok(());
        }
        if poll == max_polls {
            break;
        }
        for service in services.values_mut() {
            service.vm.pump();
        }
        for (name, service) in services.iter() {
            let actual_decisions =
                machine_replay_decisions(&service.vm.machine_execution_trace()?, control_only);
            let expected_decisions = machine_replay_decisions(&expected[name], control_only);
            if actual_decisions < expected_decisions {
                service
                    .vm
                    .wait_for_machine_execution_progress(positions[name])?;
            }
        }
    }
    let pending = services
        .iter()
        .map(|(name, service)| {
            let actual =
                machine_replay_decisions(&service.vm.machine_execution_trace()?, control_only);
            let expected = machine_replay_decisions(&expected[name], control_only);
            Ok(format!("{name}:{actual}/{expected}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Err(format!(
        "machine execution replay did not reach its retained cut ({})",
        pending.join(", ")
    ))
}

fn machine_replay_decisions(trace: &[String], control_only: bool) -> usize {
    if control_only {
        machine_replay_control_trace(trace).len()
    } else {
        trace.len()
    }
}

fn machine_replay_control_trace(trace: &[String]) -> Vec<&str> {
    trace
        .iter()
        .filter(|record| record.starts_with("host:"))
        .map(String::as_str)
        .collect()
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
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &mut BTreeMap<String, SharedSimSwitch>,
    max_rounds: u64,
) -> Result<u64, String> {
    let mut barrier_rounds: u64 = 0;
    if service.paused_until == Some(round) {
        if service.vm.exited().is_none() {
            service.vm.resume()?;
            service.faults.push(AppliedFault {
                round,
                kind: "resume".to_owned(),
                detail: "pause duration elapsed".to_owned(),
                barrier_rounds: None,
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
                barrier_rounds: None,
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
                    barrier_rounds: None,
                });
            }
            FaultKind::Restart => {
                service.record_network_traffic()?;
                service.record_network_trace()?;
                service.vm.stop();
                let serial = service_dir.join(format!("serial-{}.log", service.serial_logs.len()));
                let kernel = service_dir.join("artifacts/kernel");
                let initramfs = service_dir.join("artifacts/initramfs");
                let mut replacement = build_service(
                    name,
                    service.serial_logs.len(),
                    plan,
                    &kernel,
                    &initramfs,
                    &serial,
                    switches,
                )?;
                replacement.resume()?;
                let readiness_rounds = wait_for_serial_with_service_rounds(
                    &serial,
                    b"THES:M:42",
                    "restarted service serial readiness",
                    0,
                    &mut replacement,
                    services,
                    switches,
                    max_rounds,
                )?;
                let event_rounds = inject_serial_events_with_service_rounds(
                    &mut replacement,
                    &plan.run.events,
                    &serial,
                    services,
                    switches,
                    max_rounds.saturating_sub(readiness_rounds),
                )?;
                service.vm = replacement;
                service.serial_logs.push(serial);
                service.faults.push(AppliedFault {
                    round,
                    kind: "restart".to_owned(),
                    detail: "cold-restarted from locked service artifacts".to_owned(),
                    barrier_rounds: Some(readiness_rounds.saturating_add(event_rounds)),
                });
                barrier_rounds =
                    barrier_rounds.saturating_add(readiness_rounds.saturating_add(event_rounds));
            }
            FaultKind::ClockJump => {
                let nanoseconds = fault.nanoseconds.expect("validated clock jump fault");
                service.vm.jump_virtual_time(nanoseconds)?;
                service.faults.push(AppliedFault {
                    round,
                    kind: "clock_jump".to_owned(),
                    detail: format!("moved virtual clock by {nanoseconds} ns"),
                    barrier_rounds: None,
                });
            }
        }
    }
    Ok(barrier_rounds)
}

fn inject_serial_events_with_service_rounds(
    service: &mut ServiceVm,
    events: &[EventPlan],
    serial_log: &Path,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    max_rounds: u64,
) -> Result<u64, String> {
    let mut rounds: u64 = 0;
    for event in events {
        let input_offset = fs::metadata(serial_log)
            .map_err(|error| {
                format!(
                    "cannot inspect serial log {}: {error}",
                    serial_log.display()
                )
            })?
            .len() as usize;
        service.push_serial_input(&decode_hex(&event.data_hex)?)?;
        if let Some(checkpoint) = &event.checkpoint {
            let used = wait_for_serial_with_service_rounds(
                serial_log,
                checkpoint.as_bytes(),
                "campaign operation checkpoint",
                input_offset,
                service,
                services,
                switches,
                max_rounds.saturating_sub(rounds),
            )?;
            rounds = rounds.saturating_add(used);
        }
    }
    Ok(rounds)
}

fn wait_for_serial_with_service_rounds(
    serial_log: &Path,
    needle: &[u8],
    purpose: &str,
    input_offset: usize,
    service: &mut ServiceVm,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    max_rounds: u64,
) -> Result<u64, String> {
    for round in 0..=max_rounds {
        reject_active_replay_divergence(services)?;
        if fs::read(serial_log)
            .is_ok_and(|serial| serial_marker_after(&serial, input_offset, needle))
        {
            return Ok(round);
        }
        if round == max_rounds {
            break;
        }
        advance_topology_round_with_target(service, services, switches)?;
    }
    Err(format!(
        "service did not announce {purpose} within {max_rounds} topology rounds: {}",
        serial_log.display()
    ))
}

/// Drive the target held outside `services` and every peer through one
/// deterministic topology round. Lifecycle restart barriers use this while a
/// replacement VM is not yet part of the service map.
fn advance_topology_round_with_target(
    target: &mut ServiceVm,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
) -> Result<(), String> {
    target.pump();
    for service in services.values_mut() {
        service.vm.pump();
    }
    target.advance_simulated_networks()?;
    advance_network_round(switches, services)
}

fn serial_marker_after(serial: &[u8], input_offset: usize, needle: &[u8]) -> bool {
    serial
        .get(input_offset..)
        .is_some_and(|response| serial_marker_line_end(response, needle).is_some())
}

/// A marker is complete only after its line terminator has reached the host
/// log. Observing the marker bytes alone can race the UART's trailing CR/LF,
/// producing a result digest for a file that is still growing.
fn serial_marker_line_end(response: &[u8], needle: &[u8]) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    response
        .windows(needle.len())
        .enumerate()
        .find_map(|(marker_offset, window)| {
            if window != needle {
                return None;
            }
            let marker_end = marker_offset + needle.len();
            let suffix = &response[marker_end..];
            let newline = suffix.iter().position(|byte| *byte == b'\n')?;
            suffix[..newline]
                .iter()
                .all(|byte| *byte == b'\r')
                .then_some((marker_offset, marker_end + newline + 1))
        })
}

fn inject_campaign_events(
    driver_name: &str,
    driver: &mut ServiceRuntime,
    events: &[EventPlan],
    serial_log: &Path,
    topology: &TopologyPlan,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    round: &mut u64,
    recorded: &mut Vec<AppliedCampaignAction>,
) -> Result<Vec<CampaignUartBarrier>, String> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let mut barriers = Vec::with_capacity(events.len());
    for event in events {
        let input_offset = fs::metadata(serial_log)
            .map_err(|error| {
                format!(
                    "cannot inspect serial log {}: {error}",
                    serial_log.display()
                )
            })?
            .len() as usize;
        driver.vm.push_serial_input(&decode_hex(&event.data_hex)?)?;
        if let Some(checkpoint) = &event.checkpoint {
            barriers.push(wait_for_serial_after_rounds(
                serial_log,
                input_offset,
                checkpoint.as_bytes(),
                "campaign operation checkpoint",
                driver,
                services,
                switches,
                round,
            )?);
        } else {
            barriers.push(CampaignUartBarrier::default());
        }
        for action in &event.actions {
            recorded.push(apply_campaign_action(
                action,
                driver_name,
                driver,
                topology,
                services,
                switches,
                round,
            )?);
        }
    }
    Ok(barriers)
}

/// Inject one prefix-tree operation and drive the complete simulated topology
/// in numbered rounds until its post-input marker arrives.  A campaign prefix
/// cannot use host elapsed time as its response budget: a network-dependent
/// guest needs the switch and every simulated NIC to advance while it waits.
fn inject_campaign_operation(
    driver_name: &str,
    driver: &mut ServiceRuntime,
    serial_log: &Path,
    topology: &TopologyPlan,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    event: &EventPlan,
    round: &mut u64,
    recorded: &mut Vec<AppliedCampaignAction>,
) -> Result<CampaignUartBarrier, String> {
    let input_offset = fs::metadata(serial_log)
        .map_err(|error| {
            format!(
                "cannot inspect serial log {}: {error}",
                serial_log.display()
            )
        })?
        .len() as usize;
    driver.vm.push_serial_input(&decode_hex(&event.data_hex)?)?;
    let barrier = match &event.checkpoint {
        Some(checkpoint) => wait_for_serial_after_rounds(
            serial_log,
            input_offset,
            checkpoint.as_bytes(),
            "campaign operation checkpoint",
            driver,
            services,
            switches,
            round,
        )?,
        None => CampaignUartBarrier::default(),
    };
    for action in &event.actions {
        recorded.push(apply_campaign_action(
            action,
            driver_name,
            driver,
            topology,
            services,
            switches,
            round,
        )?);
    }
    Ok(barrier)
}

fn apply_campaign_action(
    action: &CampaignAction,
    driver_name: &str,
    driver: &mut ServiceRuntime,
    topology: &TopologyPlan,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    round: &mut u64,
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
        CampaignFaultKind::LinkFault
        | CampaignFaultKind::LinkRecover
        | CampaignFaultKind::LinkClog
        | CampaignFaultKind::LinkUnclog => {
            let network = action
                .network
                .as_deref()
                .ok_or_else(|| "campaign directed link fault has no network".to_owned())?;
            let from = action
                .from
                .as_deref()
                .ok_or_else(|| "campaign directed link fault has no source".to_owned())?;
            let to = action
                .to
                .as_deref()
                .ok_or_else(|| "campaign directed link fault has no destination".to_owned())?;
            let recover = matches!(
                action.kind,
                CampaignFaultKind::LinkRecover | CampaignFaultKind::LinkUnclog
            );
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
                driver.vm.set_network_link_conditions(
                    network,
                    &destination,
                    (!recover).then_some(action),
                )?;
            } else {
                services
                    .get(from)
                    .ok_or_else(|| format!("campaign link source did not start: {from}"))?
                    .vm
                    .set_network_link_conditions(
                        network,
                        &destination,
                        (!recover).then_some(action),
                    )?;
            }
            Ok(AppliedCampaignAction {
                operation: action.operation.clone(),
                kind: match action.kind {
                    CampaignFaultKind::LinkRecover => "link_recover",
                    CampaignFaultKind::LinkUnclog => "link_unclog",
                    CampaignFaultKind::LinkClog => "link_clog",
                    _ => "link_fault",
                }
                .to_owned(),
                target: format!("network:{network}/{from}->{to}"),
                detail: if recover {
                    "restored the directed link".to_owned()
                } else if matches!(action.kind, CampaignFaultKind::LinkClog) {
                    format!(
                        "clogged the directed link with latency_rounds={}",
                        action.latency_rounds.unwrap_or(0),
                    )
                } else {
                    format!(
                        "drop_ppm={}, duplicate_ppm={}, corrupt_ppm={}, latency_rounds={}, jitter_rounds={}, tx_bytes_per_round={}, mtu_bytes={}, tx_queue_frames={}, rx_queue_frames={}",
                        action.drop_ppm.unwrap_or(0),
                        action.duplicate_ppm.unwrap_or(0),
                        action.corrupt_ppm.unwrap_or(0),
                        action.latency_rounds.unwrap_or(0),
                        action.jitter_rounds.unwrap_or(0),
                        action.tx_bytes_per_round.unwrap_or(0),
                        action.mtu_bytes.unwrap_or(0),
                        action.tx_queue_frames.unwrap_or(0),
                        action.rx_queue_frames.unwrap_or(0),
                    )
                },
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
        CampaignFaultKind::ServiceStop
        | CampaignFaultKind::ServiceStart
        | CampaignFaultKind::ServiceKill
        | CampaignFaultKind::ServiceRestart => {
            let service = action
                .service
                .as_deref()
                .ok_or_else(|| "campaign service action has no service".to_owned())?;
            let verb = match action.kind {
                CampaignFaultKind::ServiceStop => "stop",
                CampaignFaultKind::ServiceStart => "start",
                CampaignFaultKind::ServiceKill => "kill",
                CampaignFaultKind::ServiceRestart => "restart",
                _ => unreachable!(),
            };
            apply_service_process_action(
                service,
                verb,
                &action.operation,
                driver_name,
                driver,
                services,
                switches,
                round,
            )?;
            Ok(AppliedCampaignAction {
                operation: action.operation.clone(),
                kind: format!("service_{verb}"),
                target: format!("service:{service}"),
                detail: format!("service process group {verb} completed at the operation barrier"),
            })
        }
        CampaignFaultKind::CpuThrottle | CampaignFaultKind::CpuRelease => {
            let service_name = action
                .service
                .as_deref()
                .ok_or_else(|| "campaign cpu action has no service".to_owned())?;
            let release = matches!(action.kind, CampaignFaultKind::CpuRelease);
            let target = if service_name == driver_name {
                driver
            } else {
                services.get_mut(service_name).ok_or_else(|| {
                    format!("campaign cpu action service did not start: {service_name}")
                })?
            };
            let detail = if release {
                target.throttle = None;
                "released the cpu throttle at the operation barrier".to_owned()
            } else {
                let duration = action
                    .duration_rounds
                    .ok_or_else(|| "campaign cpu_throttle has no duration_rounds".to_owned())?;
                let every_n = action
                    .every_n_rounds
                    .ok_or_else(|| "campaign cpu_throttle has no every_n_rounds".to_owned())?;
                target.throttle = Some(CpuThrottleState {
                    until_round: round.saturating_add(duration),
                    every_n_rounds: every_n,
                });
                format!("throttled to 1 of {every_n} rounds for {duration} rounds")
            };
            Ok(AppliedCampaignAction {
                operation: action.operation.clone(),
                kind: if release { "cpu_release" } else { "cpu_throttle" }.to_owned(),
                target: format!("service:{service_name}"),
                detail,
            })
        }
        CampaignFaultKind::Pause | CampaignFaultKind::Restart | CampaignFaultKind::ClockJump => {
            Err("campaign lifecycle fault cannot be applied at an operation barrier".to_owned())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_service_process_action(
    service_name: &str,
    verb: &str,
    operation: &str,
    driver_name: &str,
    driver: &mut ServiceRuntime,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    round: &mut u64,
) -> Result<(), String> {
    let name = format!("fault_{operation}_{service_name}_{verb}");
    let command = serde_json::to_string(&serde_json::json!({
        "name": &name,
        "action": verb,
    }))
    .expect("service action serializes");
    let bytes = format!("THES:SERVICE:action:{command}\n").into_bytes();
    let checkpoint = format!("THES:CHECKPOINT:{name}");
    let pass = format!("THES:SERVICE:action:{name}:PASS");

    if service_name == driver_name {
        let serial = driver.serial_logs[0].clone();
        let offset = fs::metadata(&serial)
            .map_err(|error| error.to_string())?
            .len() as usize;
        driver.vm.push_serial_input(&bytes)?;
        wait_for_serial_after_rounds(
            &serial,
            offset,
            checkpoint.as_bytes(),
            "service action checkpoint",
            driver,
            services,
            switches,
            round,
        )?;
        let response = fs::read(&serial).map_err(|error| error.to_string())?;
        if !response[offset..]
            .windows(pass.len())
            .any(|window| window == pass.as_bytes())
        {
            return Err(format!("service {service_name:?} failed to {verb}"));
        }
        return Ok(());
    }

    let mut target = services
        .remove(service_name)
        .ok_or_else(|| format!("campaign service action target did not start: {service_name}"))?;
    let serial = target.serial_logs[0].clone();
    let offset = fs::metadata(&serial)
        .map_err(|error| error.to_string())?
        .len() as usize;
    let result = (|| {
        target.vm.push_serial_input(&bytes)?;
        let max_rounds = campaign_barrier_round_limit(bytes.len());
        for step in 0..=max_rounds {
            if fs::read(&serial).is_ok_and(|serial| {
                serial[offset..]
                    .windows(checkpoint.len())
                    .any(|window| window == checkpoint.as_bytes())
            }) {
                let response = fs::read(&serial).map_err(|error| error.to_string())?;
                return response[offset..]
                    .windows(pass.len())
                    .any(|window| window == pass.as_bytes())
                    .then_some(())
                    .ok_or_else(|| format!("service {service_name:?} failed to {verb}"));
            }
            if step == max_rounds || *round == u64::MAX {
                break;
            }
            *round += 1;
            target.vm.pump();
            target.vm.advance_simulated_networks()?;
            driver.vm.pump();
            driver.vm.advance_simulated_networks()?;
            for service in services.values_mut() {
                service.vm.pump();
                service.vm.advance_simulated_networks()?;
            }
            for switch in switches.values() {
                switch
                    .lock()
                    .map_err(|_| "simulated switch lock poisoned".to_owned())?
                    .advance_round();
            }
        }
        Err(format!(
            "service {service_name:?} did not acknowledge {verb} within {max_rounds} topology rounds"
        ))
    })();
    services.insert(service_name.to_owned(), target);
    result
}

fn wait_for_serial_with_topology_rounds(
    serial_log: &Path,
    needle: &[u8],
    purpose: &str,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    max_rounds: u64,
) -> Result<u64, String> {
    for round in 0..=max_rounds {
        reject_active_replay_divergence(services)?;
        if fs::read(serial_log)
            .is_ok_and(|serial| serial.windows(needle.len()).any(|window| window == needle))
        {
            return Ok(round);
        }
        if round == max_rounds {
            break;
        }
        for service in services.values_mut() {
            service.vm.pump();
        }
        advance_network_round(switches, services)?;
    }
    Err(format!(
        "service did not announce {purpose} within {max_rounds} topology rounds (network={}): {}",
        network_timeout_evidence(services),
        serial_log.display()
    ))
}

fn network_timeout_evidence(services: &BTreeMap<String, ServiceRuntime>) -> String {
    let evidence = services
        .iter()
        .map(|(name, service)| {
            let traffic = service.vm.network_traffic();
            let traces = service.vm.network_trace().map(|networks| {
                networks
                    .into_iter()
                    .map(|(network, frames)| {
                        let frames = frames
                            .into_iter()
                            .map(|frame| {
                                serde_json::json!({
                                    "round": frame.round,
                                    "direction": frame.direction,
                                    "drop_reason": frame.drop_reason,
                                    "bytes": frame.data_hex.len() / 2,
                                    "ethertype": frame.data_hex.get(24..28).unwrap_or("unknown"),
                                })
                            })
                            .collect::<Vec<_>>();
                        (network, frames)
                    })
                    .collect::<BTreeMap<_, _>>()
            });
            (
                name,
                serde_json::json!({
                    "traffic": traffic.unwrap_or_default(),
                    "trace": traces.unwrap_or_default(),
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    serde_json::to_string(&evidence).unwrap_or_else(|error| format!("unavailable:{error}"))
}

/// Resume services in a deterministic topological order. A pivot emits its
/// boot marker only after its declared image-service readiness checks pass;
/// a plain image emits it immediately before its unmodified entrypoint. That
/// gives `service_healthy` and `service_started` their respective Compose
/// meanings without host-time polling or guest-side wait scripts.
fn start_services_in_dependency_order(
    topology: &TopologyPlan,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    max_rounds: u64,
) -> Result<u64, String> {
    let order = dependency_startup_order(topology)?;
    let mut rounds = 0;
    for name in order {
        let service = services
            .get(name.as_str())
            .ok_or_else(|| format!("dependency service did not start: {name}"))?;
        service.vm.resume()?;
        let serial = service.serial_logs[0].clone();
        let remaining = max_rounds.saturating_sub(rounds);
        rounds = rounds.saturating_add(wait_for_serial_with_topology_rounds(
            &serial,
            b"THES:M:42",
            "dependency startup",
            services,
            switches,
            remaining,
        )?);
    }
    Ok(rounds)
}

fn dependency_startup_order(topology: &TopologyPlan) -> Result<Vec<String>, String> {
    fn visit(
        name: &str,
        topology: &TopologyPlan,
        active: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) -> Result<(), String> {
        if complete.contains(name) {
            return Ok(());
        }
        let service = topology
            .services
            .get(name)
            .ok_or_else(|| format!("dependency references unknown service {name:?}"))?;
        if !active.insert(name.to_owned()) {
            return Err(format!(
                "Compose depends_on graph contains a cycle at service {name:?}"
            ));
        }
        for dependency in &service.depends_on {
            let target = topology.services.get(&dependency.service).ok_or_else(|| {
                format!(
                    "service {name:?} depends on unknown service {:?}",
                    dependency.service
                )
            })?;
            if dependency.condition == DependencyCondition::ServiceHealthy
                && target.run.container_service.is_none()
                && target.healthcheck.is_none()
            {
                return Err(format!(
                    "service {name:?} requires healthy dependency {:?}, but it has no Compose healthcheck or container_service readiness contract",
                    dependency.service
                ));
            }
            visit(&dependency.service, topology, active, complete, order)?;
        }
        active.remove(name);
        complete.insert(name.to_owned());
        order.push(name.to_owned());
        Ok(())
    }

    let mut active = BTreeSet::new();
    let mut complete = BTreeSet::new();
    let mut order = Vec::with_capacity(topology.services.len());
    for name in topology.services.keys() {
        visit(name, topology, &mut active, &mut complete, &mut order)?;
    }
    Ok(order)
}

// A barrier covers two independent kinds of guest work: delivering the command
// through the UART and running the operation that produces the checkpoint.  In
// particular, an image-backed shell operation can consume the complete serial
// input before it starts fork/exec, filesystem I/O, or a bounded virtual-time
// wait.  Keep a substantial fixed execution allowance in addition to the
// input-sized delivery allowance.
const CAMPAIGN_BARRIER_BASE_ROUNDS: u64 = 4096;
const CAMPAIGN_BARRIER_MAX_ROUNDS: u64 = 16384;
const CAMPAIGN_BARRIER_ROUNDS_PER_INPUT_BYTE: u64 = 32;

fn campaign_barrier_round_limit(input_bytes: usize) -> u64 {
    CAMPAIGN_BARRIER_BASE_ROUNDS
        .saturating_add(
            u64::try_from(input_bytes)
                .unwrap_or(u64::MAX)
                .saturating_mul(CAMPAIGN_BARRIER_ROUNDS_PER_INPUT_BYTE),
        )
        .min(CAMPAIGN_BARRIER_MAX_ROUNDS)
}

/// Drive every service and simulated network once.  The target is held outside
/// the service map while a prefix operation is injected, so keep it explicit.
fn advance_campaign_operation_round(
    target: &mut ServiceRuntime,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
) -> Result<(), String> {
    let replay_position = target.vm.machine_execution_replay_position()?;
    target.vm.pump();
    target.vm.advance_simulated_networks()?;
    for service in services.values_mut() {
        service.vm.pump();
        service.vm.advance_simulated_networks()?;
    }
    for switch in switches.values() {
        switch
            .lock()
            .map_err(|_| "simulated switch lock poisoned".to_owned())?
            .advance_round();
    }
    // Exact replay can deliberately park a vCPU at an asynchronous virtio
    // interrupt turn. Do not let the host-side round loop exhaust its entire
    // deterministic budget before the device worker gets scheduled and
    // publishes that completion.
    target
        .vm
        .wait_for_machine_execution_progress(replay_position)?;
    Ok(())
}

/// Wait for a post-input UART barrier using deterministic topology rounds rather
/// than a host-time deadline. This lets a response that needs simulated
/// network delivery complete while giving every campaign the same bound.
fn wait_for_serial_after_rounds(
    serial_log: &Path,
    input_offset: usize,
    needle: &[u8],
    purpose: &str,
    target: &mut ServiceRuntime,
    services: &mut BTreeMap<String, ServiceRuntime>,
    switches: &BTreeMap<String, SharedSimSwitch>,
    round: &mut u64,
) -> Result<CampaignUartBarrier, String> {
    let accepted_input = target.vm.serial_input_depth()?;
    let max_rounds = campaign_barrier_round_limit(accepted_input);
    for step in 0..=max_rounds {
        if let Ok(serial) = fs::read(serial_log) {
            let response = serial.get(input_offset..).unwrap_or_default();
            if let Some((marker_offset, line_end)) = serial_marker_line_end(response, needle) {
                let through_marker = &response[..line_end];
                return Ok(CampaignUartBarrier {
                    recorded: true,
                    checkpoint: String::from_utf8_lossy(needle).into_owned(),
                    marker_offset,
                    round: *round,
                    response: campaign_serial_evidence(through_marker),
                });
            }
        }
        if step == max_rounds || *round == u64::MAX {
            break;
        }
        *round += 1;
        advance_campaign_operation_round(target, services, switches)?;
    }
    let unread = target.vm.serial_input_depth()?;
    let uart = target.vm.serial_input_diagnostics()?;
    let trace = target.vm.machine_execution_trace()?;
    let trace_tail = trace.iter().rev().take(8).cloned().collect::<Vec<_>>();
    Err(format!(
        "service did not announce {purpose} within {max_rounds} topology rounds after UART input ({unread} unread UART bytes; {uart}; VM exit={:?}; decisions={}; newest decisions={trace_tail:?}): {}",
        target.vm.exited(),
        trace.len(),
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
                let serial = fs::read(path).unwrap_or_default();
                needle.is_empty() || serial.windows(needle.len()).any(|window| window == needle)
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

const TOPOLOGY_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=-1 quiet loglevel=0";

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
            // Keep the captured service stream limited to workload output.
            // Kernel timestamps and host-dependent CPU calibration values are
            // diagnostics, not deterministic replay evidence.
            boot_args: Some(TOPOLOGY_BOOT_ARGS.to_owned()),
        })
        .map_err(|error| error.to_string())?;
    resources
        .update_machine_config(&MachineConfigUpdate {
            vcpu_count: Some(service.run.run.vcpu_count),
            mem_size_mib: Some(service.run.run.mem_size_mib as usize),
            // Retain the deterministic dirty-page footprint at every branch
            // barrier. This is logical COW accounting, never a host-time
            // measurement.
            track_dirty_pages: Some(true),
            virtual_time: service
                .run
                .run
                .virtual_time
                .as_ref()
                .map(|time| VirtualTimeConfig {
                    tick_ns: time.tick_ns,
                    exits_per_tick: time.exits_per_tick as u64,
                    hold_kernel_timers: time.hold_kernel_timers,
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
    for (interface_index, network) in service.networks.iter().enumerate() {
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
                container_guest_mac(service, interface_index)?,
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

/// Container guests commonly use the same deterministic entropy seed. Letting
/// Linux synthesize a NIC address would therefore give peers the same MAC and
/// prevent ARP from establishing an ordinary service-to-service path. Derive
/// a stable locally administered address from the already locked IPv4 address.
fn container_guest_mac(
    service: &ServicePlan,
    interface_index: usize,
) -> Result<Option<MacAddr>, String> {
    let Some(network) = &service.run.container_network else {
        return Ok(None);
    };
    let interface = network
        .interfaces
        .get(interface_index)
        .ok_or_else(|| format!("container network is missing interface index {interface_index}"))?;
    let octets = interface
        .address
        .parse::<Ipv4Addr>()
        .map_err(|error| {
            format!(
                "invalid container IPv4 address {:?}: {error}",
                interface.address
            )
        })?
        .octets();
    Ok(Some(MacAddr::from([
        0x02, 0x00, octets[0], octets[1], octets[2], octets[3],
    ])))
}

fn restore_service(
    name: &str,
    instance: usize,
    service: &ServicePlan,
    kernel: &Path,
    initramfs: &Path,
    serial: &Path,
    switches: &BTreeMap<String, SharedSimSwitch>,
    execution_locations: Option<&[Vec<u64>]>,
    execution_ledgers: Option<&[ExecutionLedger]>,
    machine_execution_state: Option<&MachineExecutionState>,
    devices: Option<&vmm::checkpoint::ExecutionDeviceState>,
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
    let microvm_state = checkpoint
        .branch
        .microvm_state()
        .map_err(|error| format!("cannot restore in-memory checkpoint: {error}"))?;
    let vmm = restore_from_microvm_state(
        &InstanceInfo::default(),
        &mut event_manager,
        &get_empty_filters(),
        microvm_state,
        &LoadSnapshotParams {
            snapshot_path: PathBuf::new(),
            mem_backend: MemBackendConfig {
                backend_path: PathBuf::from(checkpoint.branch.memory_fd_path()),
                backend_type: MemBackendType::File,
            },
            track_dirty_pages: true,
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
        if let Some(execution_locations) = execution_locations {
            vmm.seed_execution_location_samples(execution_locations)
                .map_err(|error| error.to_string())?;
        }
        if let Some(execution_ledgers) = execution_ledgers {
            vmm.seed_execution_ledgers(execution_ledgers)
                .map_err(|error| error.to_string())?;
        }
        if let Some(machine_execution_state) = machine_execution_state {
            vmm.seed_machine_execution_state(machine_execution_state.clone())
                .map_err(|error| error.to_string())?;
        }
        if let Some(devices) = devices {
            vmm.restore_execution_devices(devices)
                .map_err(|error| error.to_string())?;
        }
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

/// Give image-backed Compose services a small deterministic IPv4 network.
///
/// Firecracker provides the L2 devices in the order in `ServicePlan.networks`.
/// The injected image pivot configures the matching `ethN` devices and writes
/// the peer aliases into `/etc/hosts` before it starts the image entrypoint.
/// Address allocation is part of the locked replay plan, not host state:
/// named networks sort into `10.1.0.0/24`, `10.2.0.0/24`, and so on; members
/// sort into host addresses starting at `.10`.
fn configure_container_networks(topology: &mut TopologyPlan) -> Result<(), String> {
    let mut addresses: BTreeMap<(String, String), String> = BTreeMap::new();
    for (network_index, (network, members)) in topology.networks.iter().enumerate() {
        let subnet = u8::try_from(network_index + 1)
            .map_err(|_| "Compose supports at most 255 named networks".to_owned())?;
        for (member_index, service) in members.iter().enumerate() {
            if !topology.services.contains_key(service) {
                return Err(format!(
                    "network {network:?} contains unknown service {service:?}"
                ));
            }
            let host = u8::try_from(member_index + 10)
                .map_err(|_| format!("network {network:?} supports at most 246 services"))?;
            addresses.insert(
                (service.clone(), network.clone()),
                format!("10.{subnet}.0.{host}"),
            );
        }
    }

    for (service_name, service) in &mut topology.services {
        if service.run.guest.image.is_none() {
            continue;
        }
        let mut interfaces = Vec::with_capacity(service.networks.len());
        let mut hosts = BTreeMap::new();
        for (interface_index, network) in service.networks.iter().enumerate() {
            let address = addresses
                .get(&(service_name.clone(), network.clone()))
                .ok_or_else(|| {
                    format!("service {service_name:?} is not a member of network {network:?}")
                })?
                .clone();
            interfaces.push(theseus_orchestrator::oci::ContainerNetworkInterface {
                name: format!("eth{interface_index}"),
                address,
                prefix_len: 24,
            });
            let members = topology.networks.get(network).ok_or_else(|| {
                format!("service {service_name:?} references unknown network {network:?}")
            })?;
            for peer in members {
                let peer_address = addresses
                    .get(&(peer.clone(), network.clone()))
                    .expect("network member has an address")
                    .clone();
                // If two services share more than one network, keep the first
                // interface in the service's declared deterministic order.
                hosts.entry(peer.clone()).or_insert(peer_address);
            }
        }
        // Compose `extra_hosts` is explicit service configuration, so it
        // intentionally wins over generated peer aliases.
        hosts.extend(service.extra_hosts.clone());
        service.run.container_network = Some(theseus_orchestrator::oci::ContainerNetwork {
            interfaces,
            hosts,
            hostname: service.hostname.clone(),
        });
    }
    Ok(())
}

fn artifact_at(path: PathBuf) -> Result<Artifact, String> {
    let path = fs::canonicalize(path).map_err(|error| error.to_string())?;
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    Ok(Artifact {
        path: path.display().to_string(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
    })
}

/// Resolve bundle-local artifact paths against the plan which records them.
/// Initial Compose plans still carry absolute source paths. Locked replay
/// plans use relative paths so moving or extracting the complete bundle does
/// not preserve a dependency on the machine that created it.
fn resolve_topology_artifacts(topology: &mut TopologyPlan, plan: &Path) -> Result<(), String> {
    let parent = canonical_parent(plan, "topology plan")?;
    if let Some(checkpoint) = &mut topology.starting_checkpoint {
        resolve_artifact_path(checkpoint, &parent)?;
    }
    if let Some(runner) = &mut topology.topology_runner {
        resolve_artifact_path(runner, &parent)?;
    }
    for service in topology.services.values_mut() {
        resolve_artifact_path(&mut service.run.runtime.firecracker, &parent)?;
        if let Some(adapter) = &mut service.run.runtime.image_adapter {
            resolve_artifact_path(adapter, &parent)?;
        }
        resolve_artifact_path(&mut service.run.guest.kernel, &parent)?;
        if let Some(initramfs) = &mut service.run.guest.initramfs {
            resolve_artifact_path(initramfs, &parent)?;
        }
        if let Some(image) = &mut service.run.guest.image {
            resolve_artifact_path(image, &parent)?;
        }
        for coverage in &mut service.coverage {
            resolve_artifact_path(&mut coverage.manifest, &parent)?;
            resolve_artifact_path(&mut coverage.symbols, &parent)?;
        }
    }
    Ok(())
}

fn resolve_artifact_path(artifact: &mut Artifact, parent: &Path) -> Result<(), String> {
    let path = Path::new(&artifact.path);
    if path.is_absolute() {
        return Ok(());
    }
    artifact.path = fs::canonicalize(parent.join(path))
        .map_err(|error| format!("cannot resolve artifact {}: {error}", path.display()))?
        .display()
        .to_string();
    Ok(())
}

fn write_replay_plan(path: &Path, topology: &TopologyPlan) -> Result<(), String> {
    let parent = canonical_parent(path, "replay plan")?;
    let mut value = serde_json::to_value(topology)
        .map_err(|error| format!("cannot encode replay plan: {error}"))?;
    make_artifact_paths_relative(&mut value, &parent)?;
    fs::write(
        path,
        serde_json::to_vec_pretty(&value)
            .map_err(|error| format!("cannot encode replay plan: {error}"))?,
    )
    .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn canonical_parent(path: &Path, description: &str) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::canonicalize(parent).map_err(|error| {
        format!(
            "cannot resolve {description} directory {}: {error}",
            parent.display()
        )
    })
}

fn make_artifact_paths_relative(
    value: &mut serde_json::Value,
    parent: &Path,
) -> Result<(), String> {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                make_artifact_paths_relative(value, parent)?;
            }
        }
        serde_json::Value::Object(object) => {
            if object.get("sha256").is_some_and(|value| value.is_string()) {
                if let Some(serde_json::Value::String(path)) = object.get_mut("path") {
                    let target = Path::new(path);
                    if target.is_absolute() {
                        *path = relative_path(parent, target)?.display().to_string();
                    }
                }
            } else {
                for value in object.values_mut() {
                    make_artifact_paths_relative(value, parent)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn relative_path(parent: &Path, target: &Path) -> Result<PathBuf, String> {
    for ancestor in parent.ancestors() {
        if let Ok(suffix) = target.strip_prefix(ancestor) {
            let mut relative = PathBuf::new();
            for _ in parent
                .strip_prefix(ancestor)
                .expect("ancestor belongs to parent")
                .components()
            {
                relative.push("..");
            }
            relative.push(suffix);
            if relative.as_os_str().is_empty() {
                relative.push(".");
            }
            return Ok(relative);
        }
    }
    Err(format!(
        "cannot make artifact {} relative to {}",
        target.display(),
        parent.display()
    ))
}

fn lock_service_inputs(service_dir: &Path, service: &mut ServicePlan) -> Result<(), String> {
    let runtime = lock_artifact(service_dir, "firecracker", &service.run.runtime.firecracker)?;
    let kernel = lock_artifact(service_dir, "kernel", &service.run.guest.kernel)?;
    service.run.runtime.firecracker = artifact_at(runtime)?;
    service.run.guest.kernel = artifact_at(kernel)?;

    match (&service.run.guest.initramfs, &service.run.guest.image) {
        (None, None) => {
            return Err("guest must contain exactly one of initramfs or image".to_owned());
        }
        // A replay plan keeps the original image and adapter beside the
        // already materialized initramfs. Re-lock all three inputs; replay
        // itself boots the exact cpio artifact recorded by the first run.
        (Some(initramfs), Some(image)) => {
            let adapter = service
                .run
                .runtime
                .image_adapter
                .as_ref()
                .ok_or_else(|| "guest.image requires runtime.image_adapter".to_owned())?;
            service.run.guest.initramfs = Some(artifact_at(lock_artifact(
                service_dir,
                "initramfs",
                initramfs,
            )?)?);
            service.run.guest.image = Some(artifact_at(lock_artifact(
                service_dir,
                "image.tar",
                image,
            )?)?);
            service.run.runtime.image_adapter = Some(artifact_at(lock_artifact(
                service_dir,
                "theseus-image",
                adapter,
            )?)?);
        }
        (Some(initramfs), None) => {
            service.run.guest.initramfs = Some(artifact_at(lock_artifact(
                service_dir,
                "initramfs",
                initramfs,
            )?)?);
        }
        (None, Some(image)) => {
            let adapter = service
                .run
                .runtime
                .image_adapter
                .as_ref()
                .ok_or_else(|| "guest.image requires runtime.image_adapter".to_owned())?;
            let image = lock_artifact(service_dir, "image.tar", image)?;
            let adapter = lock_artifact(service_dir, "theseus-image", adapter)?;
            let initramfs = service_dir.join("artifacts/initramfs");
            let mut command = Command::new(&adapter);
            command
                .arg("flatten")
                .arg(&image)
                .arg("--output")
                .arg(&initramfs);
            if let Some(contract) = &service.run.container_service {
                let contract_path = service_dir.join("container-service.json");
                fs::write(
                    &contract_path,
                    serde_json::to_vec_pretty(contract)
                        .map_err(|error| format!("cannot serialize service contract: {error}"))?,
                )
                .map_err(|error| format!("cannot write {}: {error}", contract_path.display()))?;
                command.arg("--service").arg(contract_path);
            }
            if let Some(network) = &service.run.container_network {
                let network_path = service_dir.join("container-network.json");
                fs::write(
                    &network_path,
                    serde_json::to_vec_pretty(network)
                        .map_err(|error| format!("cannot serialize network contract: {error}"))?,
                )
                .map_err(|error| format!("cannot write {}: {error}", network_path.display()))?;
                command.arg("--network").arg(network_path);
            }
            if !service.environment.is_empty() {
                let environment_path = service_dir.join("container-environment.json");
                fs::write(
                    &environment_path,
                    serde_json::to_vec_pretty(&service.environment).map_err(|error| {
                        format!("cannot serialize environment contract: {error}")
                    })?,
                )
                .map_err(|error| format!("cannot write {}: {error}", environment_path.display()))?;
                command.arg("--environment").arg(environment_path);
            }
            if let Some(launch) = &service.launch {
                if !launch.is_empty() {
                    let launch_path = service_dir.join("container-launch.json");
                    fs::write(
                        &launch_path,
                        serde_json::to_vec_pretty(launch).map_err(|error| {
                            format!("cannot serialize image launch contract: {error}")
                        })?,
                    )
                    .map_err(|error| format!("cannot write {}: {error}", launch_path.display()))?;
                    command.arg("--launch").arg(launch_path);
                }
            }
            if !service.configs.is_empty() {
                let configs_path = service_dir.join("container-configs.json");
                fs::write(
                    &configs_path,
                    serde_json::to_vec_pretty(&service.configs)
                        .map_err(|error| format!("cannot serialize config contract: {error}"))?,
                )
                .map_err(|error| format!("cannot write {}: {error}", configs_path.display()))?;
                command.arg("--configs").arg(configs_path);
            }
            if !service.secrets.is_empty() {
                let secrets_path = service_dir.join("container-secrets.json");
                fs::write(
                    &secrets_path,
                    serde_json::to_vec_pretty(&service.secrets)
                        .map_err(|error| format!("cannot serialize secret contract: {error}"))?,
                )
                .map_err(|error| format!("cannot write {}: {error}", secrets_path.display()))?;
                command.arg("--secrets").arg(secrets_path);
            }
            if !service.volumes.is_empty() {
                let volumes_path = service_dir.join("container-volumes.json");
                fs::write(
                    &volumes_path,
                    serde_json::to_vec_pretty(&service.volumes)
                        .map_err(|error| format!("cannot serialize volume contract: {error}"))?,
                )
                .map_err(|error| format!("cannot write {}: {error}", volumes_path.display()))?;
                command.arg("--volumes").arg(volumes_path);
            }
            if let Some(healthcheck) = &service.healthcheck {
                let healthcheck_path = service_dir.join("container-healthcheck.json");
                fs::write(
                    &healthcheck_path,
                    serde_json::to_vec_pretty(healthcheck).map_err(|error| {
                        format!("cannot serialize healthcheck contract: {error}")
                    })?,
                )
                .map_err(|error| format!("cannot write {}: {error}", healthcheck_path.display()))?;
                command.arg("--healthcheck").arg(healthcheck_path);
            }
            let status = command
                .status()
                .map_err(|error| format!("cannot start {}: {error}", adapter.display()))?;
            if !status.success() {
                return Err(format!("container image adapter exited with {status}"));
            }
            service.run.guest.image = Some(artifact_at(image)?);
            service.run.runtime.image_adapter = Some(artifact_at(adapter)?);
            service.run.guest.initramfs = Some(artifact_at(initramfs)?);
        }
    }
    for (index, coverage) in service.coverage.iter_mut().enumerate() {
        let manifest = lock_artifact(
            service_dir,
            &format!("coverage-{index:03}.json"),
            &coverage.manifest,
        )?;
        let symbols = lock_artifact(
            service_dir,
            &format!("coverage-{index:03}.debug"),
            &coverage.symbols,
        )?;
        coverage.manifest = artifact_at(manifest)?;
        coverage.symbols = artifact_at(symbols)?;
        validate_coverage_artifact(coverage)?;
    }
    Ok(())
}

fn validate_coverage_artifact(coverage: &CoverageArtifact) -> Result<(), String> {
    let manifest_path = Path::new(&coverage.manifest.path);
    let manifest: CoverageManifest = serde_json::from_slice(
        &fs::read(manifest_path)
            .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?,
    )
    .map_err(|error| {
        format!(
            "cannot parse coverage manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    let supported = matches!(
        (
            manifest.format.as_str(),
            manifest.coverage.as_str(),
            manifest.language.as_str()
        ),
        (
            "theseus-llvm-coverage-build-v1",
            "edges",
            "c" | "c++" | "rust"
        ) | ("theseus-go-coverage-build-v1", "blocks", "go")
    );
    if !supported
        || manifest.format != coverage.format
        || manifest.coverage != coverage.coverage
        || manifest.language != coverage.language
        || manifest.process != coverage.process
        || manifest.module != coverage.module
        || manifest.build_sha256 != coverage.build_sha256
        || manifest.gnu_build_id != coverage.gnu_build_id
    {
        return Err(format!(
            "coverage manifest identity changed: {}",
            manifest_path.display()
        ));
    }
    let symbol_name = Path::new(&manifest.symbols);
    let mut components = symbol_name.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(format!(
            "coverage manifest names an invalid symbol file: {}",
            manifest_path.display()
        ));
    }
    let symbol_path = Path::new(&coverage.symbols.path);
    let bytes = fs::read(symbol_path)
        .map_err(|error| format!("cannot read {}: {error}", symbol_path.display()))?;
    if !bytes
        .windows(coverage.build_sha256.len())
        .any(|window| window == coverage.build_sha256.as_bytes())
    {
        return Err(format!(
            "coverage symbols do not contain build identity {}: {}",
            coverage.build_sha256,
            symbol_path.display()
        ));
    }
    let file = object::File::parse(&*bytes)
        .map_err(|error| format!("coverage symbols are not an ELF object: {error}"))?;
    if file.format() != BinaryFormat::Elf {
        return Err(format!(
            "coverage symbols are not ELF: {}",
            symbol_path.display()
        ));
    }
    let actual_build_id = file
        .build_id()
        .map_err(|error| format!("cannot read coverage build ID: {error}"))?
        .map(hex);
    if coverage
        .gnu_build_id
        .as_ref()
        .is_some_and(|expected| actual_build_id.as_deref() != Some(expected.as_str()))
    {
        return Err(format!(
            "coverage symbols do not match the recorded GNU build ID: {}",
            symbol_path.display()
        ));
    }
    if coverage.format == "theseus-llvm-coverage-build-v1" {
        if file.section_by_name("__sancov_guards").is_none() {
            return Err(format!(
                "coverage symbols have no LLVM sanitizer guards: {}",
                symbol_path.display()
            ));
        }
        if actual_build_id.is_none() {
            return Err(format!(
                "coverage symbols have no GNU build ID: {}",
                symbol_path.display()
            ));
        }
        let callback = file.symbols().any(|symbol| {
            symbol
                .name()
                .is_ok_and(|name| name == "__sanitizer_cov_trace_pc_guard")
        });
        if !callback {
            return Err(format!(
                "coverage symbols have no Theseus edge callback: {}",
                symbol_path.display()
            ));
        }
    } else {
        if file.kind() != ObjectKind::Executable {
            return Err(format!(
                "Go coverage symbols are not a fixed-address executable: {}",
                symbol_path.display()
            ));
        }
        let callback = file.symbols().any(|symbol| {
            symbol
                .name()
                .is_ok_and(|name| name.contains("_theseusCoverageHit"))
        });
        if !callback {
            return Err(format!(
                "coverage symbols have no Theseus Go block callback: {}",
                symbol_path.display()
            ));
        }
    }
    if [".debug_info", ".debug_line"].into_iter().any(|name| {
        file.section_by_name(name)
            .is_none_or(|section| section.size() == 0)
    }) {
        return Err(format!(
            "coverage symbols have no source debug data: {}",
            symbol_path.display()
        ));
    }
    Loader::new(symbol_path)
        .map_err(|error| format!("coverage symbols have invalid debug data: {error}"))?;
    Ok(())
}

fn service_initramfs(service: &ServicePlan) -> Result<&Path, String> {
    service
        .run
        .guest
        .initramfs
        .as_ref()
        .map(|artifact| Path::new(&artifact.path))
        .ok_or_else(|| "service has no materialized initramfs".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn bare_plan_names_resolve_against_the_working_directory() {
        assert_eq!(
            canonical_parent(Path::new("plan.json"), "topology plan").unwrap(),
            fs::canonicalize(".").unwrap()
        );
    }

    #[test]
    fn topology_boot_hides_nondeterministic_kernel_diagnostics() {
        assert!(TOPOLOGY_BOOT_ARGS.contains("console=ttyS0"));
        assert!(TOPOLOGY_BOOT_ARGS.contains("quiet"));
        assert!(TOPOLOGY_BOOT_ARGS.contains("loglevel=0"));
    }

    #[test]
    fn exported_campaign_events_recover_global_service_order() {
        let service = serde_json::json!({
            "manifest": "theseus.toml",
            "networks": [],
            "run": {
                "format": "theseus-run-plan-v1",
                "manifest": "theseus.toml",
                "runtime": {"firecracker": {"path": "firecracker", "sha256": "a"}},
                "guest": {
                    "kernel": {"path": "vmlinux", "sha256": "b"},
                    "initramfs": {"path": "initramfs", "sha256": "c"}
                },
                "run": {
                    "seed": 1, "vcpu_count": 1, "mem_size_mib": 128,
                    "timeout_secs": 1, "virtual_time": null
                }
            }
        });
        let mut topology: TopologyPlan = serde_json::from_value(serde_json::json!({
            "format": "theseus-compose-plan-v1",
            "compose": "compose.yaml",
            "services": {"alpha": service.clone(), "beta": service},
            "networks": {},
            "event_order": ["alpha", "beta", "alpha"]
        }))
        .unwrap();
        let event = |data_hex: &str| EventPlan {
            data_hex: data_hex.to_owned(),
            checkpoint: None,
            actions: Vec::new(),
        };
        topology.services.get_mut("alpha").unwrap().run.events = vec![event("01"), event("03")];
        topology.services.get_mut("beta").unwrap().run.events = vec![event("02")];

        let ordered = ordered_topology_events(&topology).unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|(name, event)| (name.as_str(), event.data_hex.as_str()))
                .collect::<Vec<_>>(),
            [("alpha", "01"), ("beta", "02"), ("alpha", "03")]
        );

        topology.event_order.pop();
        assert!(ordered_topology_events(&topology)
            .unwrap_err()
            .contains("includes 1 of 2 events for service alpha"));
    }

    #[test]
    fn locked_replay_artifacts_follow_a_moved_bundle() {
        let directory = std::env::temp_dir().join(format!(
            "theseus-topology-portable-replay-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bundle = directory.join("created");
        let artifacts = bundle.join("checkpoint/services/api/artifacts");
        fs::create_dir_all(bundle.join("runs/000")).unwrap();
        fs::create_dir_all(&artifacts).unwrap();
        let runner = bundle.join("checkpoint/artifacts/theseus-topology");
        fs::create_dir_all(runner.parent().unwrap()).unwrap();
        fs::write(&runner, "runner").unwrap();
        let firecracker = artifacts.join("firecracker");
        let kernel = artifacts.join("kernel");
        let initramfs = artifacts.join("initramfs");
        fs::write(&firecracker, "firecracker").unwrap();
        fs::write(&kernel, "kernel").unwrap();
        fs::write(&initramfs, "initramfs").unwrap();
        let artifact = |path: &Path| serde_json::json!({"path": path, "sha256": "digest"});
        let topology: TopologyPlan = serde_json::from_value(serde_json::json!({
            "format": "theseus-compose-plan-v1",
            "compose": "compose.yaml",
            "topology_runner": artifact(&runner),
            "services": {
                "api": {
                    "manifest": "theseus.toml",
                    "run": {
                        "format": "theseus-run-plan-v1",
                        "manifest": "theseus.toml",
                        "runtime": {"firecracker": artifact(&firecracker)},
                        "guest": {
                            "kernel": artifact(&kernel),
                            "initramfs": artifact(&initramfs)
                        },
                        "run": {
                            "seed": 1,
                            "vcpu_count": 1,
                            "mem_size_mib": 128,
                            "timeout_secs": 1,
                            "max_rounds": 1,
                            "virtual_time": null
                        },
                        "network": {"loopback": false, "drop_ppm": 0, "partitioned": false}
                    },
                    "networks": []
                }
            },
            "networks": {}
        }))
        .unwrap();
        let plan = bundle.join("runs/000/replay-plan.json");
        write_replay_plan(&plan, &topology).unwrap();
        let encoded = fs::read_to_string(&plan).unwrap();
        assert!(encoded.contains("../../checkpoint/services/api/artifacts/kernel"));
        assert!(!encoded.contains(directory.to_str().unwrap()));

        let extracted = directory.join("extracted");
        fs::rename(&bundle, &extracted).unwrap();
        let moved_plan = extracted.join("runs/000/replay-plan.json");
        let mut replay: TopologyPlan =
            serde_json::from_slice(&fs::read(&moved_plan).unwrap()).unwrap();
        resolve_topology_artifacts(&mut replay, &moved_plan).unwrap();
        assert_eq!(
            replay.services["api"].run.guest.kernel.path,
            fs::canonicalize(extracted.join("checkpoint/services/api/artifacts/kernel"))
                .unwrap()
                .display()
                .to_string()
        );
        assert_eq!(
            replay.topology_runner.unwrap().path,
            fs::canonicalize(extracted.join("checkpoint/artifacts/theseus-topology"))
                .unwrap()
                .display()
                .to_string()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn locks_and_materializes_a_container_image_service() {
        let directory = std::env::temp_dir().join(format!(
            "theseus-topology-image-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let artifacts = directory.join("artifacts");
        fs::create_dir(&artifacts).unwrap();
        let firecracker = directory.join("firecracker");
        let kernel = directory.join("vmlinux");
        let image = directory.join("service.tar");
        let adapter = directory.join("theseus-image");
        fs::write(&firecracker, b"firecracker").unwrap();
        fs::write(&kernel, b"kernel").unwrap();
        fs::write(&image, b"container image").unwrap();
        fs::write(
            &adapter,
            "#!/bin/sh\nset -eu\n[ \"$1\" = flatten ] && [ \"$3\" = --output ] && [ \"$5\" = --service ] && [ \"$7\" = --network ] && [ \"$9\" = --environment ] && [ \"${11}\" = --launch ] && [ \"${13}\" = --configs ] && [ \"${15}\" = --secrets ] && [ \"${17}\" = --volumes ] && [ \"${19}\" = --healthcheck ]\ngrep -q '127.0.0.1:8080/health' \"$6\"\ngrep -q '10.1.0.10' \"$8\"\ngrep -q 'api.local' \"$8\"\ngrep -q 'MODE' \"${10}\"\ngrep -q 'working_dir' \"${12}\"\ngrep -q '1000' \"${12}\"\ngrep -q '/etc/worker.conf' \"${14}\"\ngrep -q '/run/secrets/token' \"${16}\"\ngrep -q '/var/lib/worker/state' \"${18}\"\ngrep -q '/bin/check' \"${20}\"\ncp \"$2\" \"$4\"\n",
        )
        .unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        let mut service = ServicePlan {
            manifest: "theseus.toml".to_owned(),
            run: RunPlan {
                format: "theseus-run-plan-v1".to_owned(),
                manifest: "theseus.toml".to_owned(),
                runtime: RuntimePlan {
                    firecracker: artifact_at(firecracker).unwrap(),
                    image_adapter: Some(artifact_at(adapter).unwrap()),
                },
                guest: GuestPlan {
                    kernel: artifact_at(kernel).unwrap(),
                    initramfs: None,
                    image: Some(artifact_at(image).unwrap()),
                },
                run: RunConfig {
                    seed: 1,
                    vcpu_count: 1,
                    mem_size_mib: 128,
                    timeout_secs: 1,
                    max_rounds: 1,
                    virtual_time: None,
                },
                network: NetworkConfig::default(),
                storage: Vec::new(),
                events: Vec::new(),
                checks: Vec::new(),
                container_service: Some(theseus_orchestrator::oci::ContainerServiceContract {
                    campaign: false,
                    ready: Some(theseus_orchestrator::oci::HttpReady {
                        url: "http://127.0.0.1:8080/health".to_owned(),
                        attempts: 3,
                        interval_millis: 10,
                    }),
                    assertions: Vec::new(),
                    operations: Vec::new(),
                    grpc_ready: None,
                    grpc_assertions: Vec::new(),
                    grpc_operations: Vec::new(),
                    shell_operations: Vec::new(),
                    network: theseus_orchestrator::oci::ContainerNetwork::default(),
                }),
                container_network: Some(theseus_orchestrator::oci::ContainerNetwork {
                    interfaces: vec![theseus_orchestrator::oci::ContainerNetworkInterface {
                        name: "eth0".to_owned(),
                        address: "10.1.0.10".to_owned(),
                        prefix_len: 24,
                    }],
                    hosts: BTreeMap::new(),
                    hostname: Some("api.local".to_owned()),
                }),
            },
            networks: Vec::new(),
            depends_on: Vec::new(),
            environment: BTreeMap::from([("MODE".to_owned(), "campaign".to_owned())]),
            launch: Some(theseus_orchestrator::oci::ContainerLaunch {
                command: Some(vec!["--serve".to_owned()]),
                entrypoint: None,
                working_dir: Some("/srv".to_owned()),
                user: Some(theseus_orchestrator::oci::ContainerUser {
                    uid: 1000,
                    gid: 1000,
                }),
                read_only: false,
                tmpfs: Vec::new(),
            }),
            configs: vec![theseus_orchestrator::oci::ContainerConfig {
                target: "/etc/worker.conf".to_owned(),
                data: b"mode=campaign\n".to_vec(),
            }],
            secrets: vec![theseus_orchestrator::oci::ContainerConfig {
                target: "/run/secrets/token".to_owned(),
                data: b"token\n".to_vec(),
            }],
            volumes: vec![theseus_orchestrator::oci::ContainerVolume {
                target: "/var/lib/worker".to_owned(),
                directories: vec![
                    "/var/lib/worker".to_owned(),
                    "/var/lib/worker/state".to_owned(),
                ],
                files: vec![theseus_orchestrator::oci::ContainerConfig {
                    target: "/var/lib/worker/state/value".to_owned(),
                    data: b"seeded\n".to_vec(),
                }],
            }],
            healthcheck: Some(theseus_orchestrator::oci::ContainerHealthcheck {
                command: vec!["/bin/check".to_owned()],
                interval_millis: 1_000,
                retries: 3,
                start_period_millis: 0,
            }),
            hostname: Some("api.local".to_owned()),
            extra_hosts: BTreeMap::from([("cache.local".to_owned(), "10.9.0.7".to_owned())]),
            faults: Vec::new(),
            coverage: Vec::new(),
        };

        lock_service_inputs(&directory, &mut service).unwrap();
        assert_eq!(
            fs::read(service_initramfs(&service).unwrap()).unwrap(),
            b"container image"
        );
        assert!(service.run.guest.image.is_some());
        assert!(service.run.runtime.image_adapter.is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn assigns_stable_ipv4_addresses_and_peer_names_to_container_services() {
        let mut topology: TopologyPlan = serde_json::from_str(
            r#"{
              "format":"theseus-compose-plan-v1", "compose":"compose.yaml",
              "services":{
                "api":{"manifest":"api/theseus.toml", "networks":["backplane"],
                  "hostname":"api.local", "extra_hosts":{"worker":"10.1.0.99","cache.local":"10.9.0.7"},
                  "depends_on":[{"service":"worker","condition":"service_started"}],
                  "run":{"format":"theseus-run-plan-v1", "manifest":"api/theseus.toml",
                    "runtime":{"firecracker":{"path":"firecracker","sha256":"a"}},
                    "guest":{"kernel":{"path":"vmlinux","sha256":"b"},"image":{"path":"api.tar","sha256":"c"}},
                    "run":{"seed":1,"vcpu_count":1,"mem_size_mib":128,"timeout_secs":1,"virtual_time":null},
                    "container_service":{"ready":{"url":"http://127.0.0.1:8080/health","attempts":1,"interval_millis":1}}
                  }},
                "worker":{"manifest":"worker/theseus.toml", "networks":["backplane","private"],
                  "run":{"format":"theseus-run-plan-v1", "manifest":"worker/theseus.toml",
                    "runtime":{"firecracker":{"path":"firecracker","sha256":"a"}},
                    "guest":{"kernel":{"path":"vmlinux","sha256":"b"},"image":{"path":"worker.tar","sha256":"c"}},
                    "run":{"seed":1,"vcpu_count":1,"mem_size_mib":128,"timeout_secs":1,"virtual_time":null}
                  }}
              },
              "networks":{"backplane":["api","worker"],"private":["worker"]}
            }"#,
        )
        .unwrap();

        configure_container_networks(&mut topology).unwrap();
        let api = topology.services["api"]
            .run
            .container_network
            .as_ref()
            .unwrap();
        assert_eq!(api.interfaces[0].name, "eth0");
        assert_eq!(api.interfaces[0].address, "10.1.0.10");
        assert_eq!(
            container_guest_mac(&topology.services["api"], 0)
                .unwrap()
                .unwrap()
                .to_string(),
            "02:00:0a:01:00:0a"
        );
        assert_eq!(api.hosts["worker"], "10.1.0.99");
        assert_eq!(api.hosts["cache.local"], "10.9.0.7");
        assert_eq!(api.hostname.as_deref(), Some("api.local"));
        let worker = topology.services["worker"]
            .run
            .container_network
            .as_ref()
            .unwrap();
        assert_eq!(worker.interfaces[0].address, "10.1.0.11");
        assert_eq!(worker.interfaces[1].address, "10.2.0.10");
        assert_ne!(
            container_guest_mac(&topology.services["api"], 0).unwrap(),
            container_guest_mac(&topology.services["worker"], 0).unwrap()
        );
        assert_eq!(worker.hosts["api"], "10.1.0.10");
        assert_eq!(
            dependency_startup_order(&topology).unwrap(),
            ["worker", "api"]
        );
    }

    #[test]
    fn certification_requires_virtual_time_for_every_service() {
        let topology: TopologyPlan = serde_json::from_str(
            r#"{
              "format":"theseus-compose-plan-v1",
              "compose":"compose.yaml",
              "services":{"api":{
                "manifest":"api/theseus.toml",
                "run":{"format":"theseus-run-plan-v1","manifest":"api/theseus.toml",
                  "runtime":{"firecracker":{"path":"firecracker","sha256":"a"}},
                  "guest":{"kernel":{"path":"vmlinux","sha256":"b"},"initramfs":{"path":"initramfs","sha256":"c"}},
                  "run":{"seed":1,"vcpu_count":1,"mem_size_mib":128,"timeout_secs":1,"virtual_time":null}
                },"networks":[]
              }},"networks":{}
            }"#,
        )
        .unwrap();

        assert!(validate_certification_plan(&topology)
            .unwrap_err()
            .contains("no virtual-time configuration"));
    }

    fn choice(operation: usize) -> CampaignOperationChoice {
        CampaignOperationChoice {
            operation,
            input: 0,
        }
    }

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
                    serial_pending_bytes: 0,
                    program_counters: Vec::new(),
                    next_fault: 0,
                    paused_until: None,
                throttle: None,
                    faults: Vec::new(),
                    network_traffic: BTreeMap::new(),
                    network_trace: BTreeMap::new(),
                    storage_sha256: BTreeMap::new(),
                    virtual_time_ns: None,
                    private_dirty_pages: None,
                    execution_locations: None,
                    execution_ledgers: None,
                    machine_execution_state: None,
                devices: None,
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
                    serial_pending_bytes: 0,
                    program_counters: Vec::new(),
                    next_fault: 0,
                    paused_until: None,
                throttle: None,
                    faults: Vec::new(),
                    network_traffic: BTreeMap::new(),
                    network_trace: BTreeMap::new(),
                    storage_sha256: BTreeMap::new(),
                    virtual_time_ns: None,
                    private_dirty_pages: None,
                    execution_locations: None,
                    execution_ledgers: None,
                    machine_execution_state: None,
                    devices: None,
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
    fn keyed_json_workflows_require_complete_service_stages() {
        let workflow: SerialWorkflow = serde_json::from_value(serde_json::json!({
            "pointers": ["/request_id"],
            "stages": [
                {"service": "api", "steps": [
                    {"fields": {"/event": "write"}},
                    {"fields": {"/event": "accepted"}}
                ]},
                {"service": "worker", "steps": [
                    {"fields": {"/event": "replicated"}},
                    {"fields": {"/event": "committed"}}
                ], "pointers": ["/source_request_id"]}
            ],
            "quantifier": "every",
            "occurs": {"exactly": 1}
        }))
        .unwrap();
        let api = b"{\"event\":\"write\",\"request_id\":\"r-17\"}\n{\"event\":\"accepted\",\"request_id\":\"r-17\"}\n";
        let worker = b"{\"event\":\"replicated\",\"source_request_id\":\"r-17\"}\n{\"event\":\"committed\",\"source_request_id\":\"r-17\"}\n";
        let values = workflow
            .stages
            .iter()
            .zip([api.as_slice(), worker.as_slice()])
            .map(|(stage, serial)| serial_workflow_stage_values(serial, &workflow, stage))
            .collect();
        assert!(serial_workflow_matches(&workflow, values));
        let incomplete = serial_workflow_stage_values(
            b"{\"event\":\"replicated\",\"source_request_id\":\"r-17\"}\n",
            &workflow,
            &workflow.stages[1],
        );
        let source = serial_workflow_stage_values(api, &workflow, &workflow.stages[0]);
        assert!(!serial_workflow_matches(
            &workflow,
            vec![source, incomplete]
        ));
        let checkpoint = CampaignCheckpoint {
            switches: BTreeMap::new(),
            services: BTreeMap::new(),
            scheduler: BTreeMap::from([
                (
                    "api".to_owned(),
                    ServiceSchedulerCheckpoint {
                        serial_contents: vec![api.to_vec()],
                        serial_pending_bytes: 0,
                        program_counters: Vec::new(),
                        next_fault: 0,
                        paused_until: None,
                throttle: None,
                        faults: Vec::new(),
                        network_traffic: BTreeMap::new(),
                        network_trace: BTreeMap::new(),
                        storage_sha256: BTreeMap::new(),
                        virtual_time_ns: None,
                        private_dirty_pages: None,
                        execution_locations: None,
                        execution_ledgers: None,
                        machine_execution_state: None,
                        devices: None,
                    },
                ),
                (
                    "worker".to_owned(),
                    ServiceSchedulerCheckpoint {
                        serial_contents: vec![worker.to_vec()],
                        serial_pending_bytes: 0,
                        program_counters: Vec::new(),
                        next_fault: 0,
                        paused_until: None,
                throttle: None,
                        faults: Vec::new(),
                        network_traffic: BTreeMap::new(),
                        network_trace: BTreeMap::new(),
                        storage_sha256: BTreeMap::new(),
                        virtual_time_ns: None,
                        private_dirty_pages: None,
                        execution_locations: None,
                        execution_ledgers: None,
                        machine_execution_state: None,
                        devices: None,
                    },
                ),
            ]),
            round: 0,
        };
        assert!(campaign_serial_workflow_matches(&checkpoint, &workflow));
        assert!(
            serial_workflow_description(&workflow).contains("worker (2 steps; /source_request_id)")
        );
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
            query: None,
            fields: BTreeMap::from([
                (
                    "/event".to_owned(),
                    serde_json::Value::String("assertion".to_owned()),
                ),
                ("/passed".to_owned(), serde_json::Value::Bool(false)),
            ]),
            where_: Vec::new(),
            arrays: Vec::new(),
            all: Vec::new(),
            any: Vec::new(),
            none: Vec::new(),
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
    fn json_serial_predicates_evaluate_rfc9535_jsonpath_queries() {
        let predicate: JsonPredicate = serde_json::from_value(serde_json::json!({
            "query": "$.checks[?@.name == \"serial\" && @.passed == true]"
        }))
        .unwrap();

        assert!(serial_matches_json_predicate(
            br#"{"event":"ready","checks":[{"name":"serial","passed":true}]}
"#,
            &predicate,
        ));
        assert!(!serial_matches_json_predicate(
            br#"{"event":"ready","checks":[{"name":"serial","passed":false}]}
"#,
            &predicate,
        ));
        let serial: SerialPredicate = serde_json::from_value(serde_json::json!({
            "json": {"query": "$.checks[?@.name == \"serial\" && @.passed == true]"}
        }))
        .unwrap();
        assert!(nested_predicate_description(&serial).contains("JSONPath"));
    }

    #[test]
    fn compound_serial_checks_allow_an_empty_plain_text_value() {
        let serial = std::env::temp_dir().join(format!(
            "theseus-topology-compound-check-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&serial, b"{\"event\":\"inspect\",\"value\":1}\n").unwrap();
        let predicate = serde_json::from_value(serde_json::json!({
            "json": {"fields": {"/event": "inspect", "/value": 1}}
        }))
        .unwrap();
        let results = evaluate_checks(
            &[CheckPlan {
                name: "counterexample".to_owned(),
                kind: CheckKind::SerialPropertyMatches,
                value: String::new(),
                contains_all: Vec::new(),
                contains_any: Vec::new(),
                contains_none: Vec::new(),
                predicate: Some(predicate),
            }],
            std::slice::from_ref(&serial),
        );
        fs::remove_file(serial).unwrap();

        assert_eq!(results[0].status, "passed");
    }

    #[test]
    fn json_serial_predicates_compose_same_event_branches() {
        let predicate: JsonPredicate = serde_json::from_value(serde_json::json!({
            "fields": {"/event": "operation"},
            "all": [
                {"where": [{"pointer": "/attempt", "greater_than_or_equal": 2}]},
                {"any": [
                    {"fields": {"/name": "write"}},
                    {"fields": {"/name": "retry"}}
                ]}
            ],
            "none": [{"fields": {"/aborted": true}}]
        }))
        .unwrap();

        assert!(serial_matches_json_predicate(
            br#"{"event":"operation","name":"write","attempt":2}
"#,
            &predicate,
        ));
        assert!(!serial_matches_json_predicate(
            br#"{"event":"operation","name":"read","attempt":2}
"#,
            &predicate,
        ));
        assert!(!serial_matches_json_predicate(
            br#"{"event":"operation","name":"retry","attempt":2,"aborted":true}
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
            query: None,
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
            all: Vec::new(),
            any: Vec::new(),
            none: Vec::new(),
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
        let mut campaign = CampaignPlan {
            driver: "api".to_owned(),
            fault_profile: None,
            test_template: None,
            test_templates: Vec::new(),
            max_parallel_commands: 2,
            guidance: CampaignGuidance::Coverage,
            coverage: CampaignCoverage::ExecutionLocations,
            state: BTreeMap::from([("phase".to_owned(), "idle".to_owned())]),
            operations: vec![
                CampaignOperation {
                    name: "write".to_owned(),
                    test_template: None,
                    service: "api".to_owned(),
                    command: None,
                    test_command_path: None,
                    shell_phase: None,
                    shell_process: None,
                    thread_schedule: Vec::new(),
                    choice_bounds: BTreeMap::new(),
                    thread_schedule_search: None,
                    thread_schedule_exploration: None,
                    input_hex: None,
                    inputs: vec![
                        CampaignOperationInput {
                            name: "alpha".to_owned(),
                            input_hex: "777269746520616c7068610a".to_owned(),
                            thread_schedule: Vec::new(),
                            choices: BTreeMap::new(),
                            input_template: None,
                            input_captures: BTreeMap::new(),
                            requires: Vec::new(),
                            excludes: Vec::new(),
                            max_uses: None,
                            requires_state: BTreeMap::from([(
                                "phase".to_owned(),
                                "idle".to_owned(),
                            )]),
                            sets_state: BTreeMap::from([(
                                "phase".to_owned(),
                                "written".to_owned(),
                            )]),
                        },
                        CampaignOperationInput {
                            name: "beta".to_owned(),
                            input_hex: "777269746520626574610a".to_owned(),
                            thread_schedule: Vec::new(),
                            choices: BTreeMap::new(),
                            input_template: None,
                            input_captures: BTreeMap::new(),
                            requires: vec![CampaignOperationInputReference {
                                operation: "write".to_owned(),
                                input: Some("alpha".to_owned()),
                            }],
                            excludes: Vec::new(),
                            max_uses: Some(1),
                            requires_state: BTreeMap::from([(
                                "phase".to_owned(),
                                "written".to_owned(),
                            )]),
                            sets_state: BTreeMap::from([("phase".to_owned(), "beta".to_owned())]),
                        },
                    ],
                    input_grammar: None,
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
                    max_uses: Some(2),
                    requires_state: BTreeMap::new(),
                    sets_state: BTreeMap::new(),
                },
                CampaignOperation {
                    name: "read".to_owned(),
                    test_template: None,
                    service: "api".to_owned(),
                    command: None,
                    test_command_path: None,
                    shell_phase: None,
                    shell_process: None,
                    thread_schedule: Vec::new(),
                    choice_bounds: BTreeMap::new(),
                    thread_schedule_search: None,
                    thread_schedule_exploration: None,
                    input_hex: Some("726561640a".to_owned()),
                    inputs: Vec::new(),
                    input_grammar: None,
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
                    requires_state: BTreeMap::new(),
                    sets_state: BTreeMap::new(),
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
                    serial_pending_bytes: 0,
                    program_counters: vec![0x8000],
                    next_fault: 0,
                    paused_until: None,
                throttle: None,
                    faults: Vec::new(),
                    network_traffic: BTreeMap::new(),
                    network_trace: BTreeMap::new(),
                    storage_sha256: BTreeMap::new(),
                    virtual_time_ns: None,
                    private_dirty_pages: None,
                    execution_locations: None,
                    execution_ledgers: None,
                    machine_execution_state: None,
                    devices: None,
                },
            )]),
            round: 0,
        };
        let captured_checkpoint = CampaignCheckpoint {
            scheduler: BTreeMap::from([
                (
                    "api".to_owned(),
                    ServiceSchedulerCheckpoint {
                    serial_contents: vec![
                        b"{\"event\":\"started\",\"request_id\":\"first\"}\n{\"event\":\"write\",\"request_id\":\"first\"}\n{\"event\":\"started\",\"request_id\":\"latest\"}\n{\"event\":\"write\",\"request_id\":\"latest\"}\n"
                            .to_vec(),
                    ],
                    serial_pending_bytes: 0,
                    program_counters: Vec::new(),
                    next_fault: 0,
                    paused_until: None,
                throttle: None,
                    faults: Vec::new(),
                    network_traffic: BTreeMap::new(),
                    network_trace: BTreeMap::new(),
                    storage_sha256: BTreeMap::new(),
                    virtual_time_ns: None,
                    private_dirty_pages: None,
                    execution_locations: None,
                    execution_ledgers: None,
                    machine_execution_state: None,
                devices: None,
                },
                ),
                (
                    "auditor".to_owned(),
                    ServiceSchedulerCheckpoint {
                        serial_contents: vec![
                            b"{\"event\":\"audit\",\"write_request_id\":\"first\"}\n{\"event\":\"audit\",\"write_request_id\":\"latest\"}\n"
                                .to_vec(),
                        ],
                        serial_pending_bytes: 0,
                        program_counters: Vec::new(),
                        next_fault: 0,
                        paused_until: None,
                throttle: None,
                        faults: Vec::new(),
                        network_traffic: BTreeMap::new(),
                        network_trace: BTreeMap::new(),
                        storage_sha256: BTreeMap::new(),
                        virtual_time_ns: None,
                        private_dirty_pages: None,
                    execution_locations: None,
                    execution_ledgers: None,
                    machine_execution_state: None,
                devices: None,
                },
                ),
            ]),
            switches: BTreeMap::new(),
            services: BTreeMap::new(),
            round: 0,
        };
        let captured_input = CampaignOperationInput {
            name: "default".to_owned(),
            input_hex: String::new(),
            thread_schedule: Vec::new(),
            choices: BTreeMap::new(),
            input_template: Some("retry {request}\n".to_owned()),
            input_captures: BTreeMap::from([(
                "request".to_owned(),
                CampaignOperationInputCapture {
                    service: None,
                    pointer: "/request_id".to_owned(),
                    json: Some(JsonPredicate {
                        query: None,
                        fields: BTreeMap::from([(
                            "/event".to_owned(),
                            serde_json::Value::String("write".to_owned()),
                        )]),
                        where_: Vec::new(),
                        arrays: Vec::new(),
                        all: Vec::new(),
                        any: Vec::new(),
                        none: Vec::new(),
                        capture: BTreeMap::new(),
                        equals_capture: BTreeMap::new(),
                    }),
                    sequence: Vec::new(),
                    workflow: None,
                    encoding: CampaignOperationInputEncoding::Text,
                    select: CampaignOperationInputSelect::Latest,
                },
            )]),
            requires: Vec::new(),
            excludes: Vec::new(),
            max_uses: None,
            requires_state: BTreeMap::new(),
            sets_state: BTreeMap::new(),
        };
        assert_eq!(
            campaign_operation_input_hex(
                &campaign,
                &campaign.operations[0],
                &captured_checkpoint,
                &captured_input,
            )
            .unwrap(),
            "7265747279206c61746573740a"
        );
        let mut sequenced_input: CampaignOperationInput =
            serde_json::from_value(serde_json::json!({
                "name": "default",
                "input_hex": "",
                "input_template": "retry {request}\n",
                "input_captures": {
                    "request": {
                        "pointer": "/request_id",
                        "sequence": [
                            {"json": {
                                "fields": {"/event": "started"},
                                "capture": {"request": "/request_id"}
                            }},
                            {"json": {
                                "fields": {"/event": "write"},
                                "equals_capture": {"/request_id": "request"}
                            }}
                        ]
                    }
                }
            }))
            .unwrap();
        assert_eq!(
            campaign_operation_input_hex(
                &campaign,
                &campaign.operations[0],
                &captured_checkpoint,
                &sequenced_input,
            )
            .unwrap(),
            "7265747279206c61746573740a"
        );
        sequenced_input
            .input_captures
            .get_mut("request")
            .unwrap()
            .select = CampaignOperationInputSelect::First;
        assert_eq!(
            campaign_operation_input_hex(
                &campaign,
                &campaign.operations[0],
                &captured_checkpoint,
                &sequenced_input,
            )
            .unwrap(),
            "72657472792066697273740a"
        );
        let workflow_input: CampaignOperationInput = serde_json::from_value(serde_json::json!({
            "name": "default",
            "input_hex": "",
            "input_template": "retry {request}\n",
            "input_captures": {
                "request": {
                    "pointer": "/write_request_id",
                    "workflow": {
                        "pointers": ["/request_id"],
                        "stages": [
                            {"service": "api", "steps": [
                                {"fields": {"/event": "started"}},
                                {"fields": {"/event": "write"}}
                            ]},
                            {"service": "auditor", "pointers": ["/write_request_id"], "steps": [
                                {"fields": {"/event": "audit"}}
                            ]}
                        ]
                    }
                }
            }
        }))
        .unwrap();
        assert_eq!(
            campaign_operation_input_hex(
                &campaign,
                &campaign.operations[0],
                &captured_checkpoint,
                &workflow_input,
            )
            .unwrap(),
            "7265747279206c61746573740a"
        );
        let mut first_input = captured_input.clone();
        first_input
            .input_captures
            .get_mut("request")
            .unwrap()
            .select = CampaignOperationInputSelect::First;
        assert_eq!(
            campaign_operation_input_hex(
                &campaign,
                &campaign.operations[0],
                &captured_checkpoint,
                &first_input,
            )
            .unwrap(),
            "72657472792066697273740a"
        );
        let mut encoded_input = captured_input.clone();
        encoded_input.input_template = Some("retry {request}\n".to_owned());
        encoded_input
            .input_captures
            .get_mut("request")
            .unwrap()
            .encoding = CampaignOperationInputEncoding::Hex;
        assert_eq!(
            campaign_operation_input_hex(
                &campaign,
                &campaign.operations[0],
                &captured_checkpoint,
                &encoded_input,
            )
            .unwrap(),
            "7265747279203663363137343635373337340a"
        );
        assert_eq!(
            campaign_input_value(
                serde_json::json!({"retry": true, "modes": ["normal", "force"]}),
                CampaignOperationInputEncoding::Json,
            ),
            Some("{\"modes\":[\"normal\",\"force\"],\"retry\":true}".to_owned())
        );

        assert_eq!(
            campaign_operation_choice_name(
                &campaign,
                CampaignOperationChoice {
                    operation: 0,
                    input: 1,
                },
            ),
            "write[beta]"
        );
        assert_eq!(
            campaign_operation_histories(&campaign)
                .into_iter()
                .filter(|history| history.len() == 1)
                .map(|history| campaign_operation_choice_name(&campaign, history[0]))
                .collect::<Vec<_>>(),
            vec!["write[alpha]"]
        );
        assert!(campaign_operation_histories(&campaign)
            .iter()
            .any(|history| {
                history
                    .iter()
                    .map(|choice| campaign_operation_choice_name(&campaign, *choice))
                    .collect::<Vec<_>>()
                    == ["write[alpha]", "write[beta]"]
            }));
        assert_eq!(
            campaign_state_after(&campaign, &[choice(0)])["phase"],
            "written"
        );
        assert_eq!(
            campaign_state_after(
                &campaign,
                &[
                    choice(0),
                    CampaignOperationChoice {
                        operation: 0,
                        input: 1,
                    },
                ],
            )["phase"],
            "beta"
        );
        let mut beta_fault = campaign_fault(CampaignFaultKind::Partition);
        beta_fault.after = None;
        beta_fault.after_input = Some(CampaignOperationInputReference {
            operation: "write".to_owned(),
            input: Some("beta".to_owned()),
        });
        assert!(!campaign_fault_applies(
            &beta_fault,
            &[choice(0)],
            &campaign
        ));
        assert!(campaign_fault_applies(
            &beta_fault,
            &[CampaignOperationChoice {
                operation: 0,
                input: 1,
            }],
            &campaign,
        ));
        assert_eq!(
            campaign_fault_name(&beta_fault),
            "backplane:partition@write[beta]"
        );
        assert_eq!(
            campaign_action(&beta_fault).unwrap().operation,
            "write[beta]"
        );
        assert_eq!(
            campaign_schedule_event(
                &campaign,
                &CampaignSchedule {
                    operations: vec![CampaignOperationChoice {
                        operation: 0,
                        input: 1,
                    }],
                    faults: Vec::new(),
                    thread_schedule_prefixes: vec![Vec::new()],
                },
                0,
                &checkpoint,
            )
            .unwrap()
            .event
            .data_hex,
            "777269746520626574610a"
        );
        campaign.operations[0].service = "replica".to_owned();
        assert_eq!(
            campaign_schedule_event(
                &campaign,
                &CampaignSchedule {
                    operations: vec![choice(0)],
                    faults: Vec::new(),
                    thread_schedule_prefixes: vec![Vec::new()],
                },
                0,
                &checkpoint,
            )
            .unwrap()
            .service,
            "replica"
        );
        campaign.operations[0].service = "api".to_owned();

        assert!(campaign_operation_marker_guards_are_ready(
            &campaign,
            &checkpoint,
            choice(1)
        ));

        let mut closed = checkpoint.clone();
        closed.scheduler.get_mut("api").unwrap().serial_contents[0]
            .extend_from_slice(b"THES:M:closed\n");
        assert!(!campaign_operation_marker_guards_are_ready(
            &campaign,
            &closed,
            choice(1)
        ));

        let mut structured = campaign;
        structured.operations[1].requires_markers.clear();
        structured.operations[1].requires_serial = Some(OperationSerialGuard {
            service: Some("auditor".to_owned()),
            predicate: SerialPredicate {
                contains: None,
                matches: None,
                json: Some(JsonPredicate {
                    query: None,
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
                    all: Vec::new(),
                    any: Vec::new(),
                    none: Vec::new(),
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
            choice(1)
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
                serial_pending_bytes: 0,
                program_counters: Vec::new(),
                next_fault: 0,
                paused_until: None,
                throttle: None,
                faults: Vec::new(),
                network_traffic: BTreeMap::new(),
                network_trace: BTreeMap::new(),
                storage_sha256: BTreeMap::new(),
                virtual_time_ns: None,
                private_dirty_pages: None,
                    execution_locations: None,
                    execution_ledgers: None,
                    machine_execution_state: None,
                devices: None,
                },
        );
        assert!(campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            choice(1)
        ));
        structured.operations[1].excludes_serial_joins =
            structured.operations[1].requires_serial_joins.clone();
        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            choice(1)
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
            choice(1)
        ));
        stale.scheduler.get_mut("api").unwrap().serial_contents[0]
            .extend_from_slice(b"{\"event\":\"write\",\"request_id\":\"r-18\",\"attempt\":1}\n");
        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            choice(1)
        ));
        stale.scheduler.get_mut("auditor").unwrap().serial_contents[0]
            .extend_from_slice(b"{\"event\":\"audit\",\"request_id\":\"r-18\",\"attempt\":2}\n");
        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            choice(1)
        ));
        stale.scheduler.get_mut("auditor").unwrap().serial_contents[0]
            .extend_from_slice(b"THES:ASSERT:recovered\n");
        assert!(!campaign_operation_serial_guards_are_ready(
            &structured,
            &stale,
            choice(1)
        ));
    }

    #[test]
    fn campaign_operation_uses_its_target_for_default_guards_and_captures() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "operations": [{
                "name": "retry",
                "service": "worker",
                "inputs": [{
                    "name": "default",
                    "input_hex": "",
                    "input_template": "retry {request_id}\n",
                    "input_captures": {
                        "request_id": {
                            "pointer": "/request_id",
                            "json": {"fields": {"/event": "ready"}}
                        }
                    }
                }],
                "requires_serial": {"contains": "THES:M:ready"}
            }],
            "max_runs": 1
        }))
        .unwrap();
        let scheduler = |serial: &[u8]| ServiceSchedulerCheckpoint {
            serial_contents: vec![serial.to_vec()],
            serial_pending_bytes: 0,
            program_counters: Vec::new(),
            next_fault: 0,
            paused_until: None,
                throttle: None,
            faults: Vec::new(),
            network_traffic: BTreeMap::new(),
            network_trace: BTreeMap::new(),
            storage_sha256: BTreeMap::new(),
            virtual_time_ns: None,
            private_dirty_pages: None,
            execution_locations: None,
            execution_ledgers: None,
            machine_execution_state: None,
            devices: None,
        };
        let checkpoint = CampaignCheckpoint {
            services: BTreeMap::new(),
            scheduler: BTreeMap::from([
                ("api".to_owned(), scheduler(b"THES:M:not-ready\n")),
                (
                    "worker".to_owned(),
                    scheduler(b"THES:M:ready\n{\"event\":\"ready\",\"request_id\":\"worker-1\"}\n"),
                ),
            ]),
            switches: BTreeMap::new(),
            round: 0,
        };

        assert!(campaign_operation_serial_guards_are_ready(
            &campaign,
            &checkpoint,
            choice(0)
        ));
        let event = campaign_schedule_event(
            &campaign,
            &CampaignSchedule {
                operations: vec![choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new()],
            },
            0,
            &checkpoint,
        )
        .unwrap();
        assert_eq!(event.service, "worker");
        assert_eq!(event.event.data_hex, "726574727920776f726b65722d310a");
    }

    #[test]
    fn campaign_prefix_key_includes_barrier_actions() {
        let ordinary = vec![CampaignEvent {
            service: "api".to_owned(),
            terminate_shell_processes: Vec::new(),
            recover_faults: Vec::new(),
            event: EventPlan {
                data_hex: "70696e670a".to_owned(),
                checkpoint: Some("THES:CHECKPOINT:ping".to_owned()),
                actions: Vec::new(),
            },
        }];
        let faulted = vec![CampaignEvent {
            service: "worker".to_owned(),
            terminate_shell_processes: Vec::new(),
            recover_faults: Vec::new(),
            event: EventPlan {
                data_hex: "70696e670a".to_owned(),
                checkpoint: Some("THES:CHECKPOINT:ping".to_owned()),
                actions: vec![CampaignAction {
                    operation: "ping".to_owned(),
                    kind: CampaignFaultKind::Partition,
                    service: None,
                    duration_rounds: None,
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
                    every_n_rounds: None,
                }],
            },
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
                operations: vec![choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(0), choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[1, 2],
            &[CampaignGuidanceObservation {
                operations: vec![choice(0)],
                decision_prefix: Vec::new(),
                novel_markers: 2,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            }],
            CampaignGuidance::Coverage,
            CampaignCoverage::ExecutionLocations,
        );

        assert_eq!(selected, 0);
        assert_eq!(reason, "canonical breadth-first operation seed");
    }

    #[test]
    fn campaign_selection_extends_a_new_topology_state_prefix() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(0), choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[2],
            &[CampaignGuidanceObservation {
                operations: vec![choice(0)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: true,
                failed: false,
                property_witnesses: Vec::new(),
            }],
            CampaignGuidance::Coverage,
            CampaignCoverage::ExecutionLocations,
        );

        assert_eq!(selected, 0);
        assert_eq!(reason, "extends 1-operation prefix with new topology state");
    }

    #[test]
    fn campaign_selection_extends_a_new_instruction_location_prefix() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(0), choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[1],
            &[CampaignGuidanceObservation {
                operations: vec![choice(0)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 2,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            }],
            CampaignGuidance::Coverage,
            CampaignCoverage::ExecutionLocations,
        );

        assert_eq!(selected, 0);
        assert_eq!(
            reason,
            "extends 1-operation prefix with 2 new instruction location(s)"
        );
    }

    #[test]
    fn coverage_modes_choose_distinct_extension_histories() {
        // This is the minimal deterministic evaluation corpus: every prior
        // prefix contributes exactly one kind of primary coverage evidence.
        // With one shared candidate corpus, changing only `coverage` must
        // choose a different continuation history.
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(0), choice(3)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(1), choice(3)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(2), choice(3)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let observations = vec![
            CampaignGuidanceObservation {
                operations: vec![choice(0)],
                decision_prefix: Vec::new(),
                novel_markers: 1,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
            CampaignGuidanceObservation {
                operations: vec![choice(1)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 1,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
            CampaignGuidanceObservation {
                operations: vec![choice(2)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 1,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
        ];

        let marker_history = select_campaign_schedule(
            &schedules,
            &[0, 1, 2],
            &observations,
            CampaignGuidance::Coverage,
            CampaignCoverage::Markers,
        );
        let checkpoint_pc_history = select_campaign_schedule(
            &schedules,
            &[0, 1, 2],
            &observations,
            CampaignGuidance::Coverage,
            CampaignCoverage::CheckpointPcs,
        );
        let execution_history = select_campaign_schedule(
            &schedules,
            &[0, 1, 2],
            &observations,
            CampaignGuidance::Coverage,
            CampaignCoverage::ExecutionLocations,
        );

        assert_eq!(marker_history.0, 0);
        assert_eq!(checkpoint_pc_history.0, 1);
        assert_eq!(execution_history.0, 2);
    }

    #[test]
    fn instruction_locations_keep_service_identity() {
        assert_eq!(
            campaign_instruction_locations(&BTreeMap::from([
                (
                    "api".to_owned(),
                    vec!["0x8000".to_owned(), "0x9000".to_owned()]
                ),
                ("auditor".to_owned(), vec!["0x8000".to_owned()]),
            ])),
            vec![
                "api:0x8000".to_owned(),
                "api:0x9000".to_owned(),
                "auditor:0x8000".to_owned(),
            ]
        );
    }

    #[test]
    fn application_coverage_records_are_strict_deduplicated_and_service_scoped() {
        let digest = "0123456789abcdef".repeat(4);
        let record = format!("THES:COV:v1:worker:parser:{digest}:0x42\n");
        let edge = format!("THES:COV:v2:worker:parser:{digest}:17:0x48\n");
        let maximum_edge = format!("THES:COV:v2:worker:parser:{digest}:65535:0x4c\n");
        let serial = BTreeMap::from([
            (
                "api".to_owned(),
                format!("noise\n{record}{record}{edge}{edge}{maximum_edge}").into_bytes(),
            ),
            (
                "worker".to_owned(),
                format!(
                    "THES:COV:v1:worker:parser:{}:0x42\nTHES:COV:v2:worker:parser:{digest}:0:0x48\nTHES:COV:v2:worker:parser:{digest}:65536:0x48\n",
                    digest.to_uppercase()
                )
                .into_bytes(),
            ),
        ]);
        let blocks = campaign_application_blocks_from_serial(&serial);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks["api"].len(), 3);
        assert_eq!(blocks["api"][0].process, "worker");
        assert_eq!(blocks["api"][0].module, "parser");
        assert_eq!(blocks["api"][0].build_sha256, digest.clone());
        assert_eq!(blocks["api"][0].edge, None);
        assert_eq!(blocks["api"][0].offset, "0x42");
        assert_eq!(blocks["api"][1].edge, Some(17));
        assert_eq!(blocks["api"][1].offset, "0x48");
        assert_eq!(blocks["api"][2].edge, Some(65_535));
        assert_eq!(blocks["api"][2].offset, "0x4c");
        assert_eq!(campaign_application_block_ids(&blocks).len(), 3);
        assert!(campaign_application_block_ids(&blocks)[1].contains("edge-17:0x48"));
    }

    #[test]
    fn validates_and_joins_locked_llvm_coverage_symbols() {
        let directory = std::env::temp_dir().join(format!(
            "theseus-llvm-symbols-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("topology runner has a repository parent");
        let frontend = root.join("instrumentation/llvm/theseus-coverage-clang");
        let source = root.join("scripts/tests/llvm_coverage_fixture.c");
        let binary = directory.join("fixture");
        let symbols = directory.join("symbols");
        let compilation = Command::new(frontend)
            .args(["--process", "fixture", "--module", "command", "--symbols"])
            .arg(&symbols)
            .arg("-o")
            .arg(&binary)
            .arg(&source)
            .output()
            .unwrap();
        assert!(compilation.status.success());
        let manifest_path = binary.with_extension("theseus-coverage.json");
        let manifest: CoverageManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let symbol_path = symbols.join(&manifest.symbols);
        let mut coverage = CoverageArtifact {
            format: manifest.format,
            coverage: manifest.coverage,
            language: manifest.language,
            process: manifest.process,
            module: manifest.module,
            build_sha256: manifest.build_sha256,
            gnu_build_id: manifest.gnu_build_id,
            manifest: artifact_at(manifest_path).unwrap(),
            symbols: artifact_at(symbol_path.clone()).unwrap(),
        };
        let locked = directory.join("locked");
        fs::create_dir_all(locked.join("artifacts")).unwrap();
        coverage.manifest =
            artifact_at(lock_artifact(&locked, "coverage-000.json", &coverage.manifest).unwrap())
                .unwrap();
        coverage.symbols =
            artifact_at(lock_artifact(&locked, "coverage-000.debug", &coverage.symbols).unwrap())
                .unwrap();
        validate_coverage_artifact(&coverage).unwrap();
        assert_eq!(
            Path::new(&coverage.symbols.path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("coverage-000.debug")
        );

        let output = Command::new(&binary).arg("7").output().unwrap();
        let record = String::from_utf8_lossy(&output.stderr)
            .lines()
            .find_map(parse_application_coverage_line)
            .unwrap();
        let key = (
            "api".to_owned(),
            record.process.clone(),
            record.module.clone(),
            record.build_sha256.clone(),
        );
        let symbolizer = CampaignApplicationSymbolizer {
            entries: BTreeMap::from([(
                key,
                (
                    kernel_symbols(Path::new(&coverage.symbols.path)),
                    Loader::new(&coverage.symbols.path).unwrap(),
                ),
            )]),
        };
        let mut points = BTreeMap::from([("api".to_owned(), vec![record])]);
        symbolizer.symbolize(&mut points);
        let point = &points["api"][0];
        assert!(point.symbol.is_some());
        assert_eq!(
            point.source.as_ref().map(|source| source.file.as_str()),
            Some("llvm_coverage_fixture.c")
        );

        coverage.build_sha256 = "f".repeat(64);
        assert!(validate_coverage_artifact(&coverage)
            .unwrap_err()
            .contains("identity changed"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validates_and_joins_locked_go_coverage_symbols() {
        let directory = std::env::temp_dir().join(format!(
            "theseus-go-symbols-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let digest = "0123456789abcdef".repeat(4);
        fs::write(
            directory.join("go.mod"),
            "module example.com/coverage_fixture\n\ngo 1.19\n",
        )
        .unwrap();
        fs::write(
            directory.join("fixture.go"),
            format!(
                "package main\n\nvar buildIdentity = \"{digest}\"\n\n//go:noinline\nfunc _theseusCoverageHit(block int) int {{ return block + len(buildIdentity) }}\n\nfunc main() {{ _ = _theseusCoverageHit(0) }}\n"
            ),
        )
        .unwrap();
        let binary = directory.join("fixture");
        let compilation = Command::new("go")
            .args(["build", "-buildmode=exe"])
            .arg(format!("-ldflags=-buildid={digest}"))
            .args(["-o", "fixture", "."])
            .env("CGO_ENABLED", "0")
            .env("GOWORK", "off")
            .current_dir(&directory)
            .output()
            .unwrap();
        assert!(compilation.status.success(), "{compilation:?}");
        let symbols = directory.join("fixture.debug");
        fs::copy(&binary, &symbols).unwrap();
        let manifest_path = directory.join("fixture.theseus-coverage.json");
        fs::write(
            &manifest_path,
            format!(
                "{{\"format\":\"theseus-go-coverage-build-v1\",\"coverage\":\"blocks\",\"language\":\"go\",\"process\":\"fixture\",\"module\":\"command\",\"build_sha256\":\"{digest}\",\"symbols\":\"fixture.debug\"}}"
            ),
        )
        .unwrap();
        let mut coverage = CoverageArtifact {
            format: "theseus-go-coverage-build-v1".to_owned(),
            coverage: "blocks".to_owned(),
            language: "go".to_owned(),
            process: "fixture".to_owned(),
            module: "command".to_owned(),
            build_sha256: digest.clone(),
            gnu_build_id: None,
            manifest: artifact_at(manifest_path).unwrap(),
            symbols: artifact_at(symbols).unwrap(),
        };
        let locked = directory.join("locked");
        fs::create_dir_all(locked.join("artifacts")).unwrap();
        coverage.manifest =
            artifact_at(lock_artifact(&locked, "coverage-000.json", &coverage.manifest).unwrap())
                .unwrap();
        coverage.symbols =
            artifact_at(lock_artifact(&locked, "coverage-000.debug", &coverage.symbols).unwrap())
                .unwrap();
        validate_coverage_artifact(&coverage).unwrap();

        let bytes = fs::read(&coverage.symbols.path).unwrap();
        let file = object::File::parse(&*bytes).unwrap();
        let address = file
            .symbols()
            .find(|symbol| symbol.name().is_ok_and(|name| name == "main.main"))
            .unwrap()
            .address();
        let record = ApplicationBlock {
            process: coverage.process.clone(),
            module: coverage.module.clone(),
            build_sha256: digest,
            edge: None,
            offset: format!("0x{address:x}"),
            symbol: None,
            symbol_offset: None,
            source: None,
        };
        let symbolizer = CampaignApplicationSymbolizer {
            entries: BTreeMap::from([(
                (
                    "api".to_owned(),
                    record.process.clone(),
                    record.module.clone(),
                    record.build_sha256.clone(),
                ),
                (
                    kernel_symbols(Path::new(&coverage.symbols.path)),
                    Loader::new(&coverage.symbols.path).unwrap(),
                ),
            )]),
        };
        let mut points = BTreeMap::from([("api".to_owned(), vec![record])]);
        symbolizer.symbolize(&mut points);
        let point = &points["api"][0];
        assert_eq!(point.symbol.as_deref(), Some("main.main"));
        assert!(point
            .source
            .as_ref()
            .is_some_and(|source| source.file.ends_with("fixture.go")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn thread_schedule_records_are_strict_ordered_and_service_scoped() {
        let digest = "0123456789abcdef".repeat(4);
        let first = format!("THES:SCHED:v1:worker:ledger:{digest}:0:0:0x00000007:1:0x42\n");
        let second = format!("THES:SCHED:v1:worker:ledger:{digest}:1:1:0x00000006:2:0x46\n");
        let serial = BTreeMap::from([
            (
                "api".to_owned(),
                format!("noise\n{first}{second}{first}").into_bytes(),
            ),
            (
                "worker".to_owned(),
                format!("THES:SCHED:v1:worker:ledger:{digest}:2:1:0x00000004:2:0x48\n")
                    .into_bytes(),
            ),
        ]);
        let decisions = campaign_thread_scheduling_from_serial(&serial);
        assert_eq!(decisions["api"].len(), 3);
        assert_eq!(decisions["api"][0].decision, 0);
        assert_eq!(decisions["api"][1].selected_thread, 2);
        assert_eq!(decisions["api"][2].decision, 0);
        assert_eq!(decisions["worker"].len(), 1);

        assert!(parse_thread_scheduling_line(&format!(
            "THES:SCHED:v1:worker:ledger:{digest}:0:0:0x00000001:2:0x42"
        ))
        .is_none());
        assert!(parse_thread_scheduling_line(&format!(
            "THES:SCHED:v1:worker:ledger:{}:0:0:0x00000001:0:0x42",
            digest.to_uppercase()
        ))
        .is_none());
    }

    #[test]
    fn structured_choice_records_are_strict_ordered_and_service_scoped() {
        let serial = BTreeMap::from([
            (
                "api".to_owned(),
                b"noise\nTHES:CHOICE:mode:2:1\nTHES:CHOICE:retry:3:2\n".to_vec(),
            ),
            ("worker".to_owned(), b"THES:CHOICE:mode:2:0\n".to_vec()),
        ]);
        let choices = campaign_structured_choices_from_serial(&serial);
        assert_eq!(choices["api"].len(), 2);
        assert_eq!(choices["api"][0].ordinal, 0);
        assert_eq!(choices["api"][1].ordinal, 1);
        assert_eq!(choices["api"][1].name, "retry");
        assert_eq!(choices["api"][1].upper_exclusive, 3);
        assert_eq!(choices["api"][1].selected, 2);
        assert_eq!(choices["worker"][0].ordinal, 0);

        assert!(parse_structured_choice_line("THES:CHOICE:bad:name:2:1").is_none());
        assert!(parse_structured_choice_line("THES:CHOICE:mode:0:0").is_none());
        assert!(parse_structured_choice_line("THES:CHOICE:mode:2:2").is_none());
        assert!(parse_structured_choice_line("THES:CHOICE:mode:257:1").is_none());
    }

    #[test]
    fn structured_choice_verification_requires_the_locked_point_of_use_record() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "operations": [{
                "name": "calculate",
                "service": "api",
                "choice_bounds": {"mode": 2},
                "inputs": [{
                    "name": "mode-1",
                    "input_hex": "00",
                    "choices": {"mode": 1}
                }]
            }],
            "max_runs": 1
        }))
        .unwrap();
        let schedule = CampaignSchedule {
            operations: vec![choice(0)],
            faults: Vec::new(),
            thread_schedule_prefixes: vec![Vec::new()],
        };
        let boundary = |records: serde_json::Value| {
            serde_json::from_value::<CampaignTimelineBoundary>(serde_json::json!({
                "operation": "calculate[mode-1]",
                "service": "api",
                "round": 1,
                "new_structured_choices": records,
                "serial_sha256": {},
                "state_sha256": "state"
            }))
            .unwrap()
        };
        let decision = serde_json::json!({
            "ordinal": 0,
            "name": "mode",
            "upper_exclusive": 2,
            "selected": 1
        });

        assert!(verify_campaign_structured_choices(
            &campaign,
            &schedule,
            &[boundary(serde_json::json!({"api": [decision.clone()]}))]
        )
        .is_ok());
        assert!(verify_campaign_structured_choices(
            &campaign,
            &schedule,
            &[boundary(serde_json::json!({}))]
        )
        .unwrap_err()
        .contains("did not record assigned structured choice"));
        assert!(verify_campaign_structured_choices(
            &campaign,
            &schedule,
            &[boundary(
                serde_json::json!({"api": [decision.clone(), decision]})
            )]
        )
        .unwrap_err()
        .contains("more than once"));
    }

    #[test]
    fn thread_synchronization_records_are_strict_ordered_and_address_free() {
        let digest = "0123456789abcdef".repeat(4);
        let wait = format!("THES:SYNC:v1:worker:ledger:{digest}:0:1:wait:condition:1:-\n");
        let signal = format!("THES:SYNC:v1:worker:ledger:{digest}:1:2:signal:condition:1:1\n");
        let serial = BTreeMap::from([
            (
                "api".to_owned(),
                format!("noise\n{wait}{signal}").into_bytes(),
            ),
            ("worker".to_owned(), wait.into_bytes()),
        ]);
        let events = campaign_thread_synchronization_from_serial(&serial);
        assert_eq!(events["api"].len(), 2);
        assert_eq!(events["api"][0].operation, "wait");
        assert_eq!(events["api"][1].peer_thread, Some(1));
        assert_eq!(events["worker"][0].object, 1);

        assert!(parse_thread_synchronization_line(&format!(
            "THES:SYNC:v1:worker:ledger:{digest}:0:1:unknown:condition:1:-"
        ))
        .is_none());
        assert!(parse_thread_synchronization_line(&format!(
            "THES:SYNC:v1:worker:ledger:{digest}:0:1:wait:condition:128:-"
        ))
        .is_none());
    }

    #[test]
    fn application_block_guidance_extends_the_instrumented_prefix() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(0), choice(2)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(1), choice(2)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let observations = vec![
            CampaignGuidanceObservation {
                operations: vec![choice(0)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
            CampaignGuidanceObservation {
                operations: vec![choice(1)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 2,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
        ];
        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[0, 1],
            &observations,
            CampaignGuidance::Coverage,
            CampaignCoverage::ApplicationBlocks,
        );
        assert_eq!(selected, 1);
        assert_eq!(
            reason,
            "extends 1-operation prefix with 2 new application block(s)"
        );
        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[0, 1],
            &observations,
            CampaignGuidance::Coverage,
            CampaignCoverage::ApplicationEdges,
        );
        assert_eq!(selected, 1);
        assert_eq!(
            reason,
            "extends 1-operation prefix with 2 new application edge(s)"
        );
    }

    #[test]
    fn checkpoint_execution_locations_prefer_exit_samples_and_keep_legacy_fallback() {
        let scheduler =
            |program_counters: Vec<u64>, execution_locations| ServiceSchedulerCheckpoint {
                serial_contents: Vec::new(),
                serial_pending_bytes: 0,
                program_counters,
                next_fault: 0,
                paused_until: None,
                throttle: None,
                faults: Vec::new(),
                network_traffic: BTreeMap::new(),
                network_trace: BTreeMap::new(),
                storage_sha256: BTreeMap::new(),
                virtual_time_ns: None,
                private_dirty_pages: None,
                execution_locations,
                execution_ledgers: None,
                machine_execution_state: None,
                devices: None,
            };
        let checkpoint = CampaignCheckpoint {
            switches: BTreeMap::new(),
            services: BTreeMap::new(),
            scheduler: BTreeMap::from([
                (
                    "api".to_owned(),
                    scheduler(vec![0x1000], Some(vec![vec![0x2000, 0x1000], vec![0x2000]])),
                ),
                ("legacy".to_owned(), scheduler(vec![0x3000], None)),
            ]),
            round: 0,
        };
        assert_eq!(
            campaign_checkpoint_execution_locations(&checkpoint),
            BTreeMap::from([
                (
                    "api".to_owned(),
                    vec!["0x1000".to_owned(), "0x2000".to_owned()],
                ),
                ("legacy".to_owned(), vec!["0x3000".to_owned()]),
            ])
        );
    }

    #[test]
    fn symbolized_instruction_locations_keep_raw_address_and_function_offset() {
        let symbols = vec![
            KernelSymbol {
                address: 0x8000,
                size: 0x20,
                name: "boot_guest".to_owned(),
            },
            KernelSymbol {
                address: 0x9000,
                size: 0,
                name: "idle_loop".to_owned(),
            },
            KernelSymbol {
                address: 0xa000,
                size: 0x10,
                name: "next_function".to_owned(),
            },
        ];
        assert_eq!(
            symbolize_instruction_location("0x8007", &symbols, None),
            InstructionLocation {
                address: "0x8007".to_owned(),
                symbol: Some("boot_guest".to_owned()),
                offset: Some(7),
                source: None,
            }
        );
        assert_eq!(
            symbolize_instruction_location("0x8020", &symbols, None),
            InstructionLocation {
                address: "0x8020".to_owned(),
                symbol: None,
                offset: None,
                source: None,
            }
        );
        assert_eq!(
            symbolize_instruction_location("not-an-address", &symbols, None),
            InstructionLocation {
                address: "not-an-address".to_owned(),
                symbol: None,
                offset: None,
                source: None,
            }
        );
        assert_eq!(
            symbolize_instruction_location("0x9fff", &symbols, None),
            InstructionLocation {
                address: "0x9fff".to_owned(),
                symbol: Some("idle_loop".to_owned()),
                offset: Some(0xfff),
                source: None,
            }
        );
    }

    #[test]
    fn source_location_paths_do_not_expose_build_directories() {
        assert_eq!(
            report_source_path("kernel/init/main.c"),
            "kernel/init/main.c"
        );
        assert_eq!(
            report_source_path("/build/linux/kernel/init/main.c"),
            "main.c"
        );
    }

    #[test]
    fn adaptive_guidance_prefers_an_action_with_observed_yield() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(2), choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(2), choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let observations = vec![
            CampaignGuidanceObservation {
                operations: vec![choice(0)],
                decision_prefix: Vec::new(),
                novel_markers: 2,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
            CampaignGuidanceObservation {
                operations: vec![choice(1)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
            CampaignGuidanceObservation {
                operations: vec![choice(2)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
        ];

        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[0, 1],
            &observations,
            CampaignGuidance::Adaptive,
            CampaignCoverage::Markers,
        );

        assert_eq!(selected, 1);
        assert_eq!(
            reason,
            "canonical breadth-first seed; adaptive action reward 2000 from 1 observed run(s), exploration bonus 500"
        );
    }

    #[test]
    fn unified_guidance_extends_a_rewarding_decision_prefix() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(2), choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(2), choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let observations = vec![CampaignGuidanceObservation {
            operations: vec![choice(2)],
            decision_prefix: vec!["operation:2:0".to_owned()],
            novel_markers: 0,
            novel_instructions: 0,
            novel_checkpoint_pcs: 0,
            novel_application_blocks: 0,
            novel_structured_choices: 1,
            novel_scheduling_decisions: 1,
            novel_state: true,
            failed: false,
            property_witnesses: vec!["target_is_reachable".to_owned()],
        }];

        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[0, 1],
            &observations,
            CampaignGuidance::Unified,
            CampaignCoverage::ExecutionLocations,
        );

        assert_eq!(selected, 0);
        assert!(reason.contains("unified decision prefix shares 1 point(s)"));
        assert!(reason.contains("observed reward 106000"));
    }

    #[test]
    fn posterior_guidance_prefers_a_successful_action_with_global_evidence() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(2), choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(2), choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let observations = vec![
            CampaignGuidanceObservation {
                operations: vec![choice(0)],
                decision_prefix: Vec::new(),
                novel_markers: 1,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
            CampaignGuidanceObservation {
                operations: vec![choice(1)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
        ];

        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[0, 1],
            &observations,
            CampaignGuidance::Posterior,
            CampaignCoverage::Markers,
        );

        assert_eq!(selected, 1);
        assert_eq!(
            reason,
            "canonical breadth-first seed; posterior global action evidence: 1 yield(s), 0 miss(es), mean 666‰, uncertainty 166‰"
        );
        let estimate = campaign_posterior_estimate(
            choice(0),
            &[choice(2)],
            &observations,
            CampaignCoverage::Markers,
        );
        assert_eq!(estimate.scope, "global action");
        assert_eq!(estimate.successes, 1);
        assert_eq!(estimate.misses, 0);
        assert_eq!(estimate.score, 53_248);
    }

    #[test]
    fn property_guidance_prefers_an_action_with_a_property_witness() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(2), choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(2), choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new(), Vec::new()],
            },
        ];
        let observations = vec![
            CampaignGuidanceObservation {
                operations: vec![choice(0)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: vec!["stale_read_is_reachable".to_owned()],
            },
            CampaignGuidanceObservation {
                operations: vec![choice(1)],
                decision_prefix: Vec::new(),
                novel_markers: 0,
                novel_instructions: 0,
                novel_checkpoint_pcs: 0,
                novel_application_blocks: 0,
                novel_structured_choices: 0,
                novel_scheduling_decisions: 0,
                novel_state: false,
                failed: false,
                property_witnesses: Vec::new(),
            },
        ];

        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[0, 1],
            &observations,
            CampaignGuidance::Property,
            CampaignCoverage::ExecutionLocations,
        );

        assert_eq!(selected, 1);
        assert_eq!(
            reason,
            "canonical breadth-first seed; property global action evidence: stale_read_is_reachable; 1 witness(es), 0 miss(es), exploration bonus 750"
        );
        let estimate = campaign_property_estimate(choice(0), &[choice(2)], &observations);
        assert_eq!(estimate.scope, "global action");
        assert_eq!(
            estimate.properties,
            vec!["stale_read_is_reachable".to_owned()]
        );
        assert_eq!(estimate.witnesses, 1);
        assert_eq!(estimate.score, 100_750);
    }

    #[test]
    fn replay_rejects_a_changed_recorded_guidance_or_coverage_policy() {
        let recorded = RecordedCampaignResult {
            starting_checkpoint_sha256: None,
            guidance: Some(CampaignGuidance::Adaptive),
            coverage: Some(CampaignCoverage::Markers),
            generated_candidates: 0,
            search: None,
            runs: Vec::new(),
        };

        assert_eq!(
            verify_recorded_campaign_guidance(
                CampaignGuidance::Coverage,
                CampaignCoverage::ExecutionLocations,
                &recorded,
            ),
            Err("recorded campaign guidance differs from replay plan".to_owned())
        );
        assert_eq!(
            verify_recorded_campaign_guidance(
                CampaignGuidance::Adaptive,
                CampaignCoverage::ExecutionLocations,
                &recorded,
            ),
            Err("recorded campaign coverage differs from replay plan".to_owned())
        );
        assert_eq!(
            verify_recorded_campaign_guidance(
                CampaignGuidance::Adaptive,
                CampaignCoverage::Markers,
                &recorded,
            ),
            Ok(())
        );
    }

    #[test]
    fn campaign_selection_keeps_canonical_order_without_a_signal() {
        let schedules = vec![
            CampaignSchedule {
                operations: vec![choice(0)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new()],
            },
            CampaignSchedule {
                operations: vec![choice(1)],
                faults: Vec::new(),
                thread_schedule_prefixes: vec![Vec::new()],
            },
        ];
        let (selected, reason) = select_campaign_schedule(
            &schedules,
            &[0, 1],
            &[],
            CampaignGuidance::Coverage,
            CampaignCoverage::ExecutionLocations,
        );

        assert_eq!(selected, 0);
        assert_eq!(reason, "canonical breadth-first operation seed");
    }

    #[test]
    fn campaign_boundary_delta_only_reports_new_evidence() {
        let previous = CampaignCheckpointBoundary {
            actions: Vec::new(),
            round: 0,
            markers: vec!["booted".to_owned(), "ready".to_owned()],
            application_blocks: BTreeMap::new(),
            thread_scheduling: BTreeMap::new(),
            thread_synchronization: BTreeMap::new(),
            structured_choices: BTreeMap::new(),
            program_counters: BTreeMap::from([
                ("api".to_owned(), vec!["0x1000".to_owned()]),
                ("worker".to_owned(), vec!["0x2000".to_owned()]),
            ]),
            serial_sha256: BTreeMap::from([
                ("api".to_owned(), "api-before".to_owned()),
                ("worker".to_owned(), "worker-before".to_owned()),
            ]),
            serial_contents: BTreeMap::from([
                ("api".to_owned(), b"api before\n".to_vec()),
                ("worker".to_owned(), b"worker before\n".to_vec()),
            ]),
            serial_pending_bytes: BTreeMap::from([("api".to_owned(), 2), ("worker".to_owned(), 0)]),
            network_traffic: BTreeMap::from([(
                "api".to_owned(),
                BTreeMap::from([("backplane".to_owned(), NetworkTraffic::default())]),
            )]),
            storage_sha256: BTreeMap::new(),
            virtual_time_ns: BTreeMap::new(),
            execution_ledgers: BTreeMap::new(),
            machine_execution_ledgers: BTreeMap::new(),
        };
        let boundary = CampaignCheckpointBoundary {
            actions: Vec::new(),
            round: 1,
            markers: vec![
                "booted".to_owned(),
                "ready".to_owned(),
                "written".to_owned(),
            ],
            application_blocks: BTreeMap::new(),
            thread_scheduling: BTreeMap::new(),
            thread_synchronization: BTreeMap::new(),
            structured_choices: BTreeMap::new(),
            program_counters: BTreeMap::from([
                ("api".to_owned(), vec!["0x1000".to_owned()]),
                ("worker".to_owned(), vec!["0x2004".to_owned()]),
            ]),
            serial_sha256: BTreeMap::from([
                ("api".to_owned(), "api-before".to_owned()),
                ("worker".to_owned(), "worker-after".to_owned()),
            ]),
            serial_contents: BTreeMap::from([
                ("api".to_owned(), b"api before\n".to_vec()),
                (
                    "worker".to_owned(),
                    b"worker before\nworker after\n".to_vec(),
                ),
            ]),
            serial_pending_bytes: BTreeMap::from([("api".to_owned(), 0), ("worker".to_owned(), 1)]),
            network_traffic: BTreeMap::from([(
                "api".to_owned(),
                BTreeMap::from([(
                    "backplane".to_owned(),
                    NetworkTraffic {
                        tx_frames: 2,
                        rx_frames: 1,
                        dropped: 1,
                        duplicated: 0,
                        corrupted: 0,
                        tx_sha256: None,
                        rx_sha256: None,
                    },
                )]),
            )]),
            storage_sha256: BTreeMap::new(),
            virtual_time_ns: BTreeMap::new(),
            execution_ledgers: BTreeMap::new(),
            machine_execution_ledgers: BTreeMap::new(),
        };

        assert_eq!(
            campaign_boundary_delta(&previous, &boundary),
            (
                vec!["written".to_owned()],
                vec!["worker".to_owned()],
                vec!["worker".to_owned()],
            )
        );
        assert_eq!(
            campaign_serial_delta(&previous, &boundary),
            BTreeMap::from([(
                "worker".to_owned(),
                CampaignSerialDelta {
                    bytes: 13,
                    sha256: "ae5627046ae985f97875c5dddeb60f3bf09d039d752eb841d61baed712e34c5c"
                        .to_owned(),
                    excerpt: "worker after\\n".to_owned(),
                    omitted_bytes: 0,
                },
            )])
        );
        assert_eq!(
            campaign_network_traffic_delta(&previous, &boundary),
            BTreeMap::from([(
                "api".to_owned(),
                BTreeMap::from([(
                    "backplane".to_owned(),
                    CampaignNetworkTrafficDelta {
                        tx_frames: 2,
                        rx_frames: 1,
                        dropped: 1,
                        duplicated: 0,
                        corrupted: 0,
                    },
                )]),
            )])
        );
        assert_ne!(
            campaign_boundary_state_sha256(&previous),
            campaign_boundary_state_sha256(&boundary)
        );
    }

    #[test]
    fn campaign_serial_delta_escapes_bounds_and_replaces_non_prefix_output() {
        let previous = CampaignCheckpointBoundary {
            actions: Vec::new(),
            round: 0,
            markers: Vec::new(),
            application_blocks: BTreeMap::new(),
            thread_scheduling: BTreeMap::new(),
            thread_synchronization: BTreeMap::new(),
            structured_choices: BTreeMap::new(),
            program_counters: BTreeMap::new(),
            serial_sha256: BTreeMap::new(),
            serial_contents: BTreeMap::from([
                ("api".to_owned(), b"old output".to_vec()),
                ("worker".to_owned(), Vec::new()),
            ]),
            serial_pending_bytes: BTreeMap::new(),
            network_traffic: BTreeMap::new(),
            storage_sha256: BTreeMap::new(),
            virtual_time_ns: BTreeMap::new(),
            execution_ledgers: BTreeMap::new(),
            machine_execution_ledgers: BTreeMap::new(),
        };
        let boundary = CampaignCheckpointBoundary {
            actions: Vec::new(),
            round: 1,
            markers: Vec::new(),
            application_blocks: BTreeMap::new(),
            thread_scheduling: BTreeMap::new(),
            thread_synchronization: BTreeMap::new(),
            structured_choices: BTreeMap::new(),
            program_counters: BTreeMap::new(),
            serial_sha256: BTreeMap::new(),
            serial_contents: BTreeMap::from([
                ("api".to_owned(), b"new output".to_vec()),
                (
                    "worker".to_owned(),
                    vec![b'\n'; CAMPAIGN_EVIDENCE_EXCERPT_BYTES + 1],
                ),
            ]),
            serial_pending_bytes: BTreeMap::new(),
            network_traffic: BTreeMap::new(),
            storage_sha256: BTreeMap::new(),
            virtual_time_ns: BTreeMap::new(),
            execution_ledgers: BTreeMap::new(),
            machine_execution_ledgers: BTreeMap::new(),
        };

        let delta = campaign_serial_delta(&previous, &boundary);
        assert_eq!(delta["api"].bytes, 10);
        assert_eq!(delta["api"].excerpt, "new output");
        assert_eq!(delta["worker"].bytes, CAMPAIGN_EVIDENCE_EXCERPT_BYTES + 1);
        assert_eq!(
            delta["worker"].excerpt,
            "\\n".repeat(CAMPAIGN_EVIDENCE_EXCERPT_BYTES)
        );
        assert_eq!(delta["worker"].omitted_bytes, 1);
    }

    #[test]
    fn campaign_input_evidence_escapes_and_bounds_delivered_bytes() {
        let escaped = campaign_input_evidence("ff0a");
        assert_eq!(escaped.bytes, 2);
        assert_eq!(escaped.excerpt, "\\xff\\n");
        assert_eq!(escaped.sha256.len(), 64);
        assert_eq!(escaped.omitted_bytes, 0);

        let long = campaign_input_evidence(&hex(&vec![b'x'; CAMPAIGN_EVIDENCE_EXCERPT_BYTES + 1]));
        assert_eq!(long.bytes, CAMPAIGN_EVIDENCE_EXCERPT_BYTES + 1);
        assert_eq!(long.excerpt, "x".repeat(CAMPAIGN_EVIDENCE_EXCERPT_BYTES));
        assert_eq!(long.omitted_bytes, 1);
    }

    #[test]
    fn campaign_uart_delivery_accounts_for_queue_and_marker_barrier() {
        let boundary = |pending| CampaignCheckpointBoundary {
            actions: Vec::new(),
            round: 0,
            markers: Vec::new(),
            application_blocks: BTreeMap::new(),
            thread_scheduling: BTreeMap::new(),
            thread_synchronization: BTreeMap::new(),
            structured_choices: BTreeMap::new(),
            program_counters: BTreeMap::new(),
            serial_sha256: BTreeMap::new(),
            serial_contents: BTreeMap::new(),
            serial_pending_bytes: BTreeMap::from([("worker".to_owned(), pending)]),
            network_traffic: BTreeMap::new(),
            storage_sha256: BTreeMap::new(),
            virtual_time_ns: BTreeMap::new(),
            execution_ledgers: BTreeMap::new(),
            machine_execution_ledgers: BTreeMap::new(),
        };
        let event = EventPlan {
            data_hex: "70696e670a".to_owned(),
            checkpoint: Some("THES:M:pong".to_owned()),
            actions: Vec::new(),
        };

        let delivery = campaign_uart_delivery(
            "worker",
            &event,
            &campaign_input_evidence(&event.data_hex),
            &boundary(2),
            &boundary(1),
        );

        assert_eq!(delivery.accepted_bytes, 5);
        assert_eq!(delivery.pending_before, 2);
        assert_eq!(delivery.pending_after, 1);
        assert_eq!(delivery.guest_read_bytes, 6);
        assert_eq!(delivery.checkpoint, "THES:M:pong");
        assert!(delivery.recorded);
        assert_ne!(
            campaign_boundary_state_sha256(&boundary(2)),
            campaign_boundary_state_sha256(&boundary(1))
        );
    }

    #[test]
    fn serial_marker_after_ignores_a_historical_matching_marker() {
        let old = b"THES:M:complete\n";
        assert!(!serial_marker_after(old, old.len(), b"THES:M:complete"));
        assert!(!serial_marker_after(
            &[old.as_slice(), b"reply THES:M:complete\r"].concat(),
            old.len(),
            b"THES:M:complete"
        ));
        assert!(serial_marker_after(
            &[old.as_slice(), b"reply THES:M:complete\r\r\n"].concat(),
            old.len(),
            b"THES:M:complete"
        ));
    }

    #[test]
    fn campaign_uart_budget_reserves_operation_work_and_stays_bounded() {
        assert_eq!(campaign_barrier_round_limit(0), 4096);
        assert_eq!(campaign_barrier_round_limit(214), 10944);
        assert_eq!(campaign_barrier_round_limit(usize::MAX), 16384);
    }

    #[test]
    fn campaign_replay_verifies_guidance_evidence() {
        let actual = CampaignRun {
            index: 0,
            test_template: Some("main".to_owned()),
            operations: vec!["write".to_owned()],
            decision_trace: vec!["boundary:0:operation:write".to_owned()],
            thread_schedule_prefixes: vec![vec![0, 1]],
            fault: None,
            faults: vec!["backplane:partition@write".to_owned()],
            actions: Vec::new(),
            selection: "extends 1-operation prefix with new topology state".to_owned(),
            guidance_ledger: CampaignGuidanceLedger {
                observations: 1,
                sha256: "ledger".to_owned(),
            },
            guidance_evidence: Some(CampaignPosteriorEvidence {
                action: "write".to_owned(),
                context: Vec::new(),
                scope: "global action".to_owned(),
                successes: 1,
                misses: 0,
                mean_per_mille: 666,
                uncertainty_per_mille: 166,
                score: 53_248,
            }),
            property_witnesses: vec!["stale_read_is_reachable".to_owned()],
            timeline: vec![CampaignTimelineBoundary {
                id: "op-000-write".to_owned(),
                operation: "write".to_owned(),
                command: None,
                test_command_path: None,
                terminated_command_services: Vec::new(),
                service: "api".to_owned(),
                input: CampaignInputEvidence {
                    bytes: 6,
                    sha256: "input".to_owned(),
                    excerpt: "write\\n".to_owned(),
                    omitted_bytes: 0,
                },
                delivery: CampaignUartDelivery {
                    recorded: true,
                    accepted_bytes: 6,
                    pending_before: 0,
                    pending_after: 0,
                    guest_read_bytes: 6,
                    checkpoint: "THES:M:checkpoint".to_owned(),
                },
                barrier: CampaignUartBarrier {
                    recorded: true,
                    checkpoint: "THES:M:checkpoint".to_owned(),
                    marker_offset: 0,
                    round: 1,
                    response: campaign_serial_evidence(b"THES:M:checkpoint"),
                },
                round: 1,
                actions: Vec::new(),
                markers: vec!["checkpoint".to_owned()],
                new_markers: vec!["checkpoint".to_owned()],
                changed_program_counters: vec!["api".to_owned()],
                changed_serial: vec!["api".to_owned()],
                program_counters: BTreeMap::from([("api".to_owned(), vec!["0x8000".to_owned()])]),
                instruction_locations: BTreeMap::new(),
                application_blocks: BTreeMap::new(),
                new_application_blocks: Vec::new(),
                thread_scheduling: BTreeMap::new(),
                new_thread_scheduling_decisions: BTreeMap::new(),
                thread_synchronization: BTreeMap::new(),
                new_thread_synchronization_events: BTreeMap::new(),
                structured_choices: BTreeMap::new(),
                new_structured_choices: BTreeMap::new(),
                serial_sha256: BTreeMap::from([("api".to_owned(), "serial".to_owned())]),
                serial_delta: BTreeMap::new(),
                network_traffic_delta: BTreeMap::new(),
                changed_storage: Vec::new(),
                virtual_time_delta_ns: BTreeMap::new(),
                execution_ledgers: BTreeMap::new(),
                machine_execution_ledgers: BTreeMap::new(),
                state_sha256: "state".to_owned(),
            }],
            program_counters: BTreeMap::from([("api".to_owned(), vec!["0x8000".to_owned()])]),
            instruction_locations: BTreeMap::from([(
                "api".to_owned(),
                vec![InstructionLocation {
                    address: "0x8000".to_owned(),
                    symbol: Some("checkpoint".to_owned()),
                    offset: Some(0),
                    source: Some(InstructionSourceLocation {
                        file: "main.c".to_owned(),
                        line: 42,
                        column: None,
                    }),
                }],
            )]),
            instruction_novelty: Vec::new(),
            checkpoint_pc_novelty: Vec::new(),
            application_blocks: BTreeMap::new(),
            application_block_novelty: Vec::new(),
            thread_scheduling: BTreeMap::new(),
            thread_synchronization: BTreeMap::new(),
            structured_choices: BTreeMap::new(),
            execution_ledgers: BTreeMap::from([(
                "api".to_owned(),
                vec![ExecutionLedgerEvidence {
                    decisions: 3,
                    sha256: "execution".to_owned(),
                    tail: vec!["mmio_write:0xd0000000:1:41".to_owned()],
                }],
            )]),
            machine_execution_ledgers: BTreeMap::from([(
                "api".to_owned(),
                ExecutionLedgerEvidence {
                    decisions: 3,
                    sha256: "machine-execution".to_owned(),
                    tail: vec!["vcpu:0:mmio_write:0xd0000000:1:41".to_owned()],
                },
            )]),
            machine_execution_traces: BTreeMap::from([(
                "api".to_owned(),
                vec!["vcpu:0:mmio_write:0xd0000000:1:41".to_owned()],
            )]),
            state_sha256: "state".to_owned(),
            state_novel: true,
            status: "passed",
            novelty: vec!["checkpoint".to_owned()],
        };
        let expected = RecordedCampaignRun {
            test_template: actual.test_template.clone(),
            operations: actual.operations.clone(),
            decision_trace: actual.decision_trace.clone(),
            thread_schedule_prefixes: actual.thread_schedule_prefixes.clone(),
            fault: None,
            faults: actual.faults.clone(),
            actions: Vec::new(),
            selection: actual.selection.clone(),
            guidance_ledger: Some(actual.guidance_ledger.clone()),
            guidance_evidence: actual.guidance_evidence.clone(),
            property_witnesses: Some(actual.property_witnesses.clone()),
            timeline: actual.timeline.clone(),
            program_counters: actual.program_counters.clone(),
            instruction_locations: actual.instruction_locations.clone(),
            instruction_novelty: actual.instruction_novelty.clone(),
            checkpoint_pc_novelty: actual.checkpoint_pc_novelty.clone(),
            application_blocks: actual.application_blocks.clone(),
            application_block_novelty: actual.application_block_novelty.clone(),
            thread_scheduling: actual.thread_scheduling.clone(),
            thread_synchronization: actual.thread_synchronization.clone(),
            structured_choices: actual.structured_choices.clone(),
            execution_ledgers: actual.execution_ledgers.clone(),
            machine_execution_ledgers: actual.machine_execution_ledgers.clone(),
            machine_execution_traces: actual.machine_execution_traces.clone(),
            novelty: actual.novelty.clone(),
            state_sha256: actual.state_sha256.clone(),
            state_novel: true,
            status: actual.status.to_owned(),
        };

        assert!(campaign_replay_mismatches(&expected, &actual).is_empty());
        let mut changed_template = expected.clone();
        changed_template.test_template = Some("other".to_owned());
        assert!(campaign_replay_mismatches(&changed_template, &actual)
            .contains(&"test template".to_owned()));
        let mut changed_decision = expected.clone();
        changed_decision.decision_trace[0] = "boundary:0:operation:other".to_owned();
        assert!(campaign_replay_mismatches(&changed_decision, &actual)
            .contains(&"decision trace".to_owned()));
        let mut changed_execution = expected.clone();
        changed_execution
            .execution_ledgers
            .get_mut("api")
            .expect("api execution ledger")[0]
            .decisions += 1;
        assert!(campaign_replay_mismatches(&changed_execution, &actual)
            .contains(&"ordered KVM execution ledger".to_owned()));
        let mut changed_machine_execution = expected.clone();
        changed_machine_execution
            .machine_execution_ledgers
            .get_mut("api")
            .expect("api machine execution ledger")
            .decisions += 1;
        assert!(
            campaign_replay_mismatches(&changed_machine_execution, &actual)
                .contains(&"machine-wide execution stream".to_owned())
        );
        let mut legacy_timeline = expected.clone();
        legacy_timeline.timeline[0].service.clear();
        legacy_timeline.timeline[0].input = CampaignInputEvidence::default();
        legacy_timeline.timeline[0].delivery = CampaignUartDelivery::default();
        legacy_timeline.timeline[0].barrier = CampaignUartBarrier::default();
        assert!(campaign_replay_mismatches(&legacy_timeline, &actual).is_empty());
        let mut changed_target = expected.clone();
        changed_target.timeline[0].service = "worker".to_owned();
        assert!(campaign_replay_mismatches(&changed_target, &actual)
            .contains(&"operation-boundary timeline".to_owned()));
        let mut changed_input = expected.clone();
        changed_input.timeline[0].input.sha256 = "other-input".to_owned();
        assert!(campaign_replay_mismatches(&changed_input, &actual)
            .contains(&"operation-boundary timeline".to_owned()));
        let mut changed_delivery = expected.clone();
        changed_delivery.timeline[0].delivery.guest_read_bytes = 5;
        assert!(campaign_replay_mismatches(&changed_delivery, &actual)
            .contains(&"operation-boundary timeline".to_owned()));
        let mut changed_barrier = expected.clone();
        changed_barrier.timeline[0].barrier.marker_offset = 1;
        assert!(campaign_replay_mismatches(&changed_barrier, &actual)
            .contains(&"operation-boundary timeline".to_owned()));
        let mut changed_symbols = expected.clone();
        changed_symbols
            .instruction_locations
            .get_mut("api")
            .expect("recorded API locations")
            .first_mut()
            .expect("recorded API location")
            .source = Some(InstructionSourceLocation {
            file: "other.c".to_owned(),
            line: 7,
            column: None,
        });
        assert!(campaign_replay_mismatches(&changed_symbols, &actual)
            .contains(&"symbolized instruction locations".to_owned()));
        let mut changed_property_witnesses = expected.clone();
        changed_property_witnesses.property_witnesses = Some(vec!["different".to_owned()]);
        assert!(
            campaign_replay_mismatches(&changed_property_witnesses, &actual)
                .contains(&"property guidance evidence".to_owned())
        );
        let mut changed_posterior = expected.clone();
        changed_posterior
            .guidance_evidence
            .as_mut()
            .expect("recorded posterior evidence")
            .score = 1;
        assert!(campaign_replay_mismatches(&changed_posterior, &actual)
            .contains(&"posterior guidance evidence".to_owned()));
        let mut changed_ledger = expected.clone();
        changed_ledger
            .guidance_ledger
            .as_mut()
            .expect("recorded guidance ledger")
            .sha256 = "other-ledger".to_owned();
        assert!(campaign_replay_mismatches(&changed_ledger, &actual)
            .contains(&"guidance ledger".to_owned()));
        let mut changed_timeline = expected.clone();
        changed_timeline.timeline[0]
            .markers
            .push("other".to_owned());
        assert!(campaign_replay_mismatches(&changed_timeline, &actual)
            .contains(&"operation-boundary timeline".to_owned()));
        assert!(campaign_host_input_replay_mismatches(&expected, &actual).is_empty());
        let mut ungoverned_drift = expected.clone();
        ungoverned_drift.program_counters.get_mut("api").unwrap()[0] = "0xffffffff".to_owned();
        ungoverned_drift.timeline[0].round += 10;
        ungoverned_drift.timeline[0].barrier.round += 10;
        ungoverned_drift.timeline[0]
            .program_counters
            .get_mut("api")
            .unwrap()[0] = "0xffffffff".to_owned();
        ungoverned_drift.execution_ledgers.get_mut("api").unwrap()[0].decisions += 1;
        ungoverned_drift
            .machine_execution_ledgers
            .get_mut("api")
            .unwrap()
            .decisions += 1;
        ungoverned_drift
            .machine_execution_traces
            .get_mut("api")
            .unwrap()[0] = "vcpu:0:pio_read:0x64:1:".to_owned();
        ungoverned_drift.state_sha256 = "different-state".to_owned();
        assert!(campaign_host_input_replay_mismatches(&ungoverned_drift, &actual).is_empty());
        let mut changed_control_input = expected.clone();
        changed_control_input.timeline[0].input.sha256 = "other-input".to_owned();
        assert!(
            campaign_host_input_replay_mismatches(&changed_control_input, &actual)
                .contains(&"operation control timeline".to_owned())
        );
        let mut changed_control_trace = expected.clone();
        changed_control_trace
            .machine_execution_traces
            .get_mut("api")
            .unwrap()
            .push("host:serial_input:1:41".to_owned());
        assert!(
            campaign_host_input_replay_mismatches(&changed_control_trace, &actual)
                .contains(&"actively enforced host-input trace".to_owned())
        );
        let search = CampaignSearchEvidence::default();
        let mut ungoverned_search = search.clone();
        ungoverned_search.checkpoint.private_dirty_pages = 42;
        ungoverned_search.guidance_sha256 = "different-observations".to_owned();
        assert!(campaign_host_input_search_matches(
            &ungoverned_search,
            &search
        ));
        let mut changed_search = search.clone();
        changed_search.checkpoint.checkpoint_nodes = 1;
        assert!(!campaign_host_input_search_matches(
            &changed_search,
            &search
        ));
        let mut changed = actual;
        changed.state_sha256 = "other-state".to_owned();
        assert_eq!(
            campaign_replay_mismatches(&expected, &changed),
            vec!["topology-state coverage"]
        );
    }

    #[test]
    fn campaign_guidance_ledger_is_stable_and_sensitive_to_observations() {
        let observations = vec![CampaignGuidanceObservation {
            operations: vec![choice(0)],
            decision_prefix: Vec::new(),
            novel_markers: 1,
            novel_instructions: 0,
            novel_checkpoint_pcs: 0,
            novel_application_blocks: 0,
            novel_structured_choices: 0,
            novel_scheduling_decisions: 0,
            novel_state: false,
            failed: false,
            property_witnesses: Vec::new(),
        }];
        let first = CampaignGuidanceLedger::from_observations(&observations);
        let second = CampaignGuidanceLedger::from_observations(&observations);
        assert_eq!(first, second);
        assert_eq!(first.observations, 1);
        assert_eq!(first.sha256.len(), 64);
        let legacy_shape = serde_json::to_string(&observations).unwrap();
        assert!(!legacy_shape.contains("decision_prefix"));
        assert!(!legacy_shape.contains("novel_structured_choices"));
        assert!(!legacy_shape.contains("novel_scheduling_decisions"));

        let mut changed = observations;
        changed[0].failed = true;
        assert_ne!(first, CampaignGuidanceLedger::from_observations(&changed));
    }

    #[test]
    fn checkpoint_economics_counts_capture_reuse_and_leaf_restore_work() {
        let root = CampaignCheckpoint {
            switches: BTreeMap::new(),
            services: BTreeMap::new(),
            scheduler: BTreeMap::new(),
            round: 0,
        };
        let prefix = CampaignPrefixCheckpoint {
            checkpoint: root.clone(),
            actions: Vec::new(),
            events: Vec::new(),
            barriers: Vec::new(),
            boundaries: Vec::new(),
        };
        let tree = CampaignCheckpointTree {
            root,
            prefixes: BTreeMap::from([
                ("first".to_owned(), prefix.clone()),
                ("second".to_owned(), prefix.clone()),
                ("third".to_owned(), prefix),
            ]),
            reuses: 5,
            prefix_captures: 3,
            prefix_restores: 3,
            prefix_cow_restore_bytes: 3_072,
            retained_memory_bytes: 8_192,
            retained_private_dirty_pages: 9,
            cache_policy: checkpoint_cache::Policy::new(checkpoint_cache::PREFIX_MEMORY_BUDGET),
            prefix_evictions: 0,
        };

        assert_eq!(
            tree.economics(4, 4_096),
            CampaignCheckpointEconomics {
                root_captures: 1,
                prefix_captures: 3,
                checkpoint_nodes: 4,
                prefix_reuses: 5,
                prefix_restores: 3,
                leaf_restores: 4,
                topology_restores: 7,
                avoided_prefix_recomputations: 5,
                retained_memory_bytes: 8_192,
                shared_cow_restore_bytes: 7_168,
                private_dirty_pages: 9,
                snapshot_file_bytes: 0,
                prefix_evictions: 0,
            }
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
            required: false,
            service: None,
            network: Some("backplane".to_owned()),
            from: None,
            to: None,
            drive: None,
            after: Some("write".to_owned()),
            after_input: None,
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
            every_n_rounds: None,
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
    fn campaign_fault_selections_keep_required_faults_in_every_candidate() {
        let mut required_partition = campaign_fault(CampaignFaultKind::Partition);
        required_partition.required = true;
        let mut required_heal = campaign_fault(CampaignFaultKind::Heal);
        required_heal.required = true;
        required_heal.after = Some("read".to_owned());
        let faults = vec![
            required_partition,
            campaign_fault(CampaignFaultKind::LinkPartition),
            required_heal,
        ];

        assert_eq!(
            campaign_fault_selections(&faults, &[0, 1, 2], 3),
            vec![vec![0, 2], vec![0, 1, 2]]
        );
        assert_eq!(
            campaign_fault_selections(&faults, &[0, 1, 2], 2),
            vec![vec![0, 2]]
        );
    }

    #[test]
    fn campaign_minimization_cannot_remove_a_required_fault_trigger() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "operations": [{"name": "start", "inputs": [{"name": "default", "input_hex": ""}]}],
            "faults": [{
                "kind": "partition",
                "required": true,
                "network": "backplane",
                "after": "start"
            }],
            "max_runs": 1
        }))
        .unwrap();

        assert!(campaign_required_faults_apply(
            &campaign,
            &CampaignSchedule {
                operations: vec![choice(0)],
                faults: vec![0],
                thread_schedule_prefixes: vec![Vec::new()],
            }
        ));
        assert!(!campaign_required_faults_apply(
            &campaign,
            &CampaignSchedule {
                operations: Vec::new(),
                faults: vec![0],
                thread_schedule_prefixes: Vec::new(),
            }
        ));
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
    fn test_command_histories_require_first_and_join_parallel_processes() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "operations": [
                {
                    "name": "prepare",
                    "service": "api",
                    "command": "first",
                    "inputs": [{"name": "default", "input_hex": "00"}],
                    "max_uses": 1
                },
                {
                    "name": "start",
                    "service": "api",
                    "command": "parallel_driver",
                    "shell_phase": "launch",
                    "shell_process": "writer",
                    "inputs": [{"name": "default", "input_hex": "01"}],
                    "max_uses": 1
                },
                {
                    "name": "join",
                    "service": "api",
                    "command": "parallel_driver",
                    "shell_phase": "completion",
                    "shell_process": "writer",
                    "inputs": [{"name": "default", "input_hex": "02"}],
                    "max_uses": 1
                }
            ],
            "max_runs": 8,
            "max_faults_per_run": 0,
            "max_operations_per_run": 3
        }))
        .unwrap();

        let histories = campaign_operation_histories(&campaign);
        assert_eq!(histories, vec![vec![choice(0), choice(1), choice(2)]]);
    }

    #[test]
    fn eventually_accepts_live_drivers_and_records_every_service_to_kill() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "operations": [
                {
                    "name": "start_api",
                    "service": "api",
                    "command": "parallel_driver",
                    "shell_phase": "launch",
                    "shell_process": "writer",
                    "inputs": [{"name": "default", "input_hex": "01"}],
                    "max_uses": 1
                },
                {
                    "name": "start_worker",
                    "service": "worker",
                    "command": "parallel_driver",
                    "shell_phase": "launch",
                    "shell_process": "reader",
                    "inputs": [{"name": "default", "input_hex": "02"}],
                    "max_uses": 1
                },
                {
                    "name": "recovered",
                    "service": "api",
                    "command": "eventually",
                    "inputs": [{"name": "default", "input_hex": "03"}],
                    "max_uses": 1
                }
            ],
            "faults": [{
                "kind": "partition",
                "network": "backplane",
                "after": "start_api"
            }],
            "max_runs": 8,
            "max_faults_per_run": 1,
            "max_operations_per_run": 3
        }))
        .unwrap();

        let histories = campaign_operation_histories(&campaign);
        let live_then_eventually = vec![choice(0), choice(1), choice(2)];
        assert!(histories.contains(&live_then_eventually));
        assert_eq!(
            campaign_active_shell_process_services(&campaign, &live_then_eventually[..2]),
            ["api", "worker"]
        );
        let event = campaign_schedule_event(
            &campaign,
            &CampaignSchedule {
                operations: live_then_eventually,
                faults: vec![0],
                thread_schedule_prefixes: vec![Vec::new(); 3],
            },
            2,
            &CampaignCheckpoint {
                switches: BTreeMap::new(),
                services: BTreeMap::new(),
                scheduler: BTreeMap::new(),
                round: 0,
            },
        )
        .unwrap();
        assert_eq!(event.terminate_shell_processes, ["api", "worker"]);
        assert_eq!(event.recover_faults.len(), 1);
        assert!(matches!(
            event.recover_faults[0].kind,
            CampaignFaultKind::Heal
        ));
    }

    #[test]
    fn eventually_maps_active_faults_to_quiet_recovery_actions() {
        for (fault, recovery) in [
            (CampaignFaultKind::Partition, CampaignFaultKind::Heal),
            (
                CampaignFaultKind::LinkPartition,
                CampaignFaultKind::LinkHeal,
            ),
            (CampaignFaultKind::LinkFault, CampaignFaultKind::LinkRecover),
            (CampaignFaultKind::LinkClog, CampaignFaultKind::LinkUnclog),
            (CampaignFaultKind::CpuThrottle, CampaignFaultKind::CpuRelease),
            (
                CampaignFaultKind::ServiceStop,
                CampaignFaultKind::ServiceStart,
            ),
            (
                CampaignFaultKind::ServiceKill,
                CampaignFaultKind::ServiceStart,
            ),
            (
                CampaignFaultKind::StorageFault,
                CampaignFaultKind::StorageRecover,
            ),
            (
                CampaignFaultKind::NetworkFault,
                CampaignFaultKind::NetworkRecover,
            ),
            (
                CampaignFaultKind::PacketFault,
                CampaignFaultKind::PacketRecover,
            ),
        ] {
            let action = campaign_recovery_action(&campaign_fault(fault), "eventual")
                .unwrap()
                .unwrap();
            assert!(std::mem::discriminant(&action.kind) == std::mem::discriminant(&recovery));
            assert_eq!(action.operation, "eventual");
        }
        assert!(campaign_recovery_action(
            &campaign_fault(CampaignFaultKind::ServiceRestart),
            "eventual"
        )
        .unwrap()
        .is_none());
    }


    #[test]
    fn property_corpus_verdicts_cover_every_kind() {
        let empty: Vec<bool> = Vec::new();
        let all = vec![true, true, true];
        let none = vec![false, false];
        let mixed = vec![true, false, true];
        for kind in [
            PropertyKind::Always,
            PropertyKind::AlwaysOrUnreachable,
            PropertyKind::Sometimes,
            PropertyKind::Reachable,
        ] {
            assert!(!property_corpus_failed(kind, &all), "{kind:?} passes on all");
        }
        assert!(property_corpus_failed(PropertyKind::Always, &none));
        assert!(!property_corpus_failed(PropertyKind::Unreachable, &none));
        assert!(!property_corpus_failed(PropertyKind::Unreachable, &empty));
        for kind in [PropertyKind::Always, PropertyKind::Unreachable] {
            assert!(property_corpus_failed(kind, &mixed), "{kind:?} fails on mixed");
        }
        assert!(property_corpus_failed(PropertyKind::AlwaysOrUnreachable, &mixed));
        assert!(!property_corpus_failed(
            PropertyKind::AlwaysOrUnreachable,
            &none
        ));
        assert!(!property_corpus_failed(PropertyKind::AlwaysOrUnreachable, &empty));
        for kind in [PropertyKind::Sometimes, PropertyKind::Reachable] {
            assert!(property_corpus_failed(kind, &none), "{kind:?} fails on none");
            assert!(property_corpus_failed(kind, &empty), "{kind:?} fails on empty");
            assert!(!property_corpus_failed(kind, &mixed), "{kind:?} passes on mixed");
        }
        assert_eq!(
            property_first_failing_run(PropertyKind::AlwaysOrUnreachable, &mixed),
            Some(1)
        );
        assert_eq!(
            property_first_failing_run(PropertyKind::Sometimes, &none),
            Some(0)
        );
    }

    #[test]
    fn cpu_throttles_are_incompatible_on_one_service_and_named_by_target() {
        let mut first = campaign_fault(CampaignFaultKind::CpuThrottle);
        first.service = Some("api".to_owned());
        let mut second = campaign_fault(CampaignFaultKind::CpuThrottle);
        second.service = first.service.clone();
        assert!(!campaign_faults_compatible(&first, &second));
        second.service = Some("worker".to_owned());
        assert!(campaign_faults_compatible(&first, &second));
        let mut clog = campaign_fault(CampaignFaultKind::LinkClog);
        clog.from = Some("api".to_owned());
        clog.to = Some("worker".to_owned());
        assert_eq!(
            campaign_fault_name(&first),
            "api:cpu_throttle@write".to_owned()
        );
        assert_eq!(
            campaign_fault_name(&clog),
            "backplane:api->worker:link_clog@write".to_owned()
        );
    }

    #[test]
    fn violation_excerpts_bound_and_center_on_the_needle() {
        let mut serial = Vec::new();
        for index in 0..200 {
            serial.extend_from_slice(format!("line {index} fillertext\n").as_bytes());
        }
        serial.extend_from_slice(b"counter value: 2\n");
        for index in 0..200 {
            serial.extend_from_slice(format!("after {index} fillertext\n").as_bytes());
        }
        let excerpt = serial_violation_excerpt(&serial, Some("counter value: 2"));
        assert!(excerpt.contains("counter value: 2"));
        assert!(excerpt.starts_with('…') && excerpt.ends_with('…'));
        assert!(excerpt.chars().count() < 300, "{excerpt}");

        // A needle that never appeared falls back to the bounded log tail.
        let tail = serial_violation_excerpt(&serial, Some("absent needle"));
        assert!(tail.contains("after 199 fillertext"));
        assert!(!tail.contains("line 0 fillertext"));
    }

    #[test]
    fn campaign_progress_lines_carry_status_and_failures() {
        let operations = vec!["write".to_owned(), "read".to_owned()];
        let faults = vec!["backplane:partition@write".to_owned()];
        let line = campaign_progress_line(
            3,
            2,
            "failed",
            &operations,
            &faults,
            &["lost_update_is_unreachable"],
            7,
        );
        assert_eq!(
            line,
            r#"{"format":"theseus-progress-v1","completed":3,"index":2,"status":"failed","operations":["write","read"],"faults":["backplane:partition@write"],"failed_properties":["lost_update_is_unreachable"],"checkpoint_reuses":7}"#
        );
        let line = campaign_progress_line(1, 0, "passed", &operations, &[], &[], 0);
        assert!(line.contains(r#""status":"passed""#));
        assert!(!line.contains("failed_properties"));
    }

    #[test]
    fn cpu_throttle_gate_skips_modulus_rounds_only_inside_the_window() {
        let throttle = CpuThrottleState {
            until_round: 10,
            every_n_rounds: 3,
        };
        let skipped = |round: u64| {
            throttle.until_round > round && round % u64::from(throttle.every_n_rounds) != 0
        };
        // Rounds 1, 2, 4, 5, 7, 8 skip; rounds 0, 3, 6, 9 run; the window
        // closes at round 10 and every round runs afterwards.
        for round in [1u64, 2, 4, 5, 7, 8] {
            assert!(skipped(round), "round {round} should skip");
        }
        for round in [0u64, 3, 6, 9, 10, 11, 12] {
            assert!(!skipped(round), "round {round} should run");
        }
    }

    #[test]
    fn test_command_histories_keep_singleton_and_driver_timelines_separate() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "operations": [
                {"name": "first", "command": "first", "inputs": [{"name": "default", "input_hex": "00"}], "max_uses": 1},
                {"name": "driver", "command": "serial_driver", "inputs": [{"name": "default", "input_hex": "01"}], "max_uses": 1},
                {"name": "singleton", "command": "singleton_driver", "inputs": [{"name": "default", "input_hex": "02"}], "max_uses": 1},
                {"name": "check", "command": "anytime", "inputs": [{"name": "default", "input_hex": "03"}], "max_uses": 1},
                {"name": "final", "command": "finally", "inputs": [{"name": "default", "input_hex": "04"}], "max_uses": 1}
            ],
            "max_runs": 32,
            "max_faults_per_run": 0,
            "max_operations_per_run": 4
        }))
        .unwrap();

        let histories = campaign_operation_histories(&campaign);
        assert!(!histories.is_empty());
        assert!(histories.iter().all(|history| history[0] == choice(0)));
        assert!(histories.iter().all(|history| {
            let names = history
                .iter()
                .map(|choice| campaign.operations[choice.operation].name.as_str())
                .collect::<Vec<_>>();
            !(names.contains(&"driver") && names.contains(&"singleton"))
                && names
                    .iter()
                    .position(|name| *name == "final")
                    .is_none_or(|index| index + 1 == names.len())
        }));
        assert!(histories.iter().all(|history| {
            history.iter().any(|choice| {
                campaign.operations[choice.operation].command
                    == Some(CampaignTestCommand::SingletonDriver)
            }) || history.iter().any(|choice| {
                campaign.operations[choice.operation].command
                    == Some(CampaignTestCommand::SerialDriver)
            })
        }));
    }

    #[test]
    fn multiple_test_templates_are_selected_once_and_interleaved() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "test_templates": ["alpha", "beta"],
            "operations": [
                {"name": "alpha_write", "test_template": "alpha", "command": "parallel_driver", "inputs": [{"name": "default", "input_hex": "00"}], "max_uses": 1},
                {"name": "alpha_check", "test_template": "alpha", "command": "finally", "inputs": [{"name": "default", "input_hex": "01"}], "max_uses": 1},
                {"name": "beta_smoke", "test_template": "beta", "command": "singleton_driver", "inputs": [{"name": "default", "input_hex": "02"}], "max_uses": 1},
                {"name": "beta_check", "test_template": "beta", "command": "finally", "inputs": [{"name": "default", "input_hex": "03"}], "max_uses": 1}
            ],
            "max_runs": 16,
            "max_faults_per_run": 1,
            "max_operations_per_run": 2
        }))
        .unwrap();

        validate_campaign_test_commands(&campaign).unwrap();
        let histories = campaign_operation_histories(&campaign);
        assert!(!histories.is_empty());
        assert_eq!(
            campaign.operations[histories[0][0].operation]
                .test_template
                .as_deref(),
            Some("alpha")
        );
        assert_eq!(
            campaign.operations[histories[1][0].operation]
                .test_template
                .as_deref(),
            Some("beta")
        );
        assert!(histories.iter().all(|history| {
            let templates = history
                .iter()
                .map(|choice| {
                    campaign.operations[choice.operation]
                        .test_template
                        .as_deref()
                        .unwrap()
                })
                .collect::<BTreeSet<_>>();
            templates.len() == 1
        }));

        let schedule = CampaignSchedule {
            operations: histories[0].clone(),
            faults: Vec::new(),
            thread_schedule_prefixes: vec![Vec::new(); histories[0].len()],
        };
        assert_eq!(
            campaign_schedule_test_template(&campaign, &schedule),
            Some("alpha")
        );
    }

    #[test]
    fn campaign_schedule_search_cases_keep_their_locked_thread_choices() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "operations": [{
                "name": "race",
                "service": "api",
                "thread_schedule_search": {
                    "threads": [0, 1, 2],
                    "period": 2,
                    "max_switches": 1,
                    "generated_schedules": 3
                },
                "inputs": [
                    {"name": "schedule-0-0", "input_hex": "00", "thread_schedule": [0, 0]},
                    {"name": "schedule-0-1", "input_hex": "01", "thread_schedule": [0, 1]},
                    {"name": "schedule-0-2", "input_hex": "02", "thread_schedule": [0, 2]}
                ]
            }],
            "max_runs": 3,
            "max_faults_per_run": 1,
            "max_operations_per_run": 1
        }))
        .unwrap();

        let histories = campaign_operation_histories(&campaign);
        assert_eq!(histories.len(), 3);
        assert_eq!(
            histories
                .iter()
                .map(|history| campaign_operation_choice_name(&campaign, history[0]))
                .collect::<Vec<_>>(),
            [
                "race[schedule-0-0]",
                "race[schedule-0-1]",
                "race[schedule-0-2]"
            ]
        );
        assert_eq!(
            campaign_operation_input(&campaign, histories[1][0])
                .unwrap()
                .thread_schedule,
            [0, 1]
        );
    }

    #[test]
    fn locked_thread_schedule_matches_the_shell_command_environment() {
        let mut input = CampaignOperationInput {
            name: "schedule-0-1".to_owned(),
            input_hex: hex(
                br#"THES:SHELL:operation:{"environment":{"THESEUS_THREAD_SCHEDULE":"0,1"}}
"#,
            ),
            thread_schedule: vec![0, 1],
            choices: BTreeMap::new(),
            input_template: None,
            input_captures: BTreeMap::new(),
            requires: Vec::new(),
            excludes: Vec::new(),
            max_uses: None,
            requires_state: BTreeMap::new(),
            sets_state: BTreeMap::new(),
        };
        validate_campaign_thread_schedule_input(&input).unwrap();

        input.thread_schedule = vec![0, 2];
        assert!(validate_campaign_thread_schedule_input(&input)
            .unwrap_err()
            .contains("does not match"));
    }

    #[test]
    fn runnable_prefix_command_replaces_the_locked_empty_prefix() {
        let input = hex(
            br#"THES:SHELL:operation:{"environment":{"THESEUS_THREAD_SCHEDULE":"","THESEUS_THREAD_SCHEDULE_MODE":"runnable_prefix"}}
"#,
        );
        let changed = campaign_input_with_runnable_prefix(&input, &[0, 2, 1]).unwrap();
        let changed = String::from_utf8(decode_hex(&changed).unwrap()).unwrap();
        assert!(changed.contains("\"THESEUS_THREAD_SCHEDULE\":\"0,2,1\""));
        assert!(changed.contains("\"THESEUS_THREAD_SCHEDULE_MODE\":\"runnable_prefix\""));
    }

    #[test]
    fn runnable_prefix_search_forks_only_observed_alternatives() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "ledger",
            "operations": [{
                "name": "deposit",
                "service": "ledger",
                "thread_schedule_exploration": {
                    "strategy": "runnable_prefixes",
                    "max_choices": 8,
                    "max_variants": 4
                },
                "input_hex": "00"
            }],
            "max_runs": 4,
            "max_operations_per_run": 1
        }))
        .unwrap();
        let source = CampaignSchedule {
            operations: vec![choice(0)],
            faults: Vec::new(),
            thread_schedule_prefixes: vec![Vec::new()],
        };
        let boundary: CampaignTimelineBoundary = serde_json::from_value(serde_json::json!({
            "operation": "deposit",
            "service": "ledger",
            "round": 1,
            "new_thread_scheduling_decisions": {"ledger": [
                {"process":"ledger","module":"deposit","build_sha256":"build","decision":1,"from_thread":0,"runnable_mask":"0x00000006","selected_thread":1,"point_offset":"0x10"},
                {"process":"ledger","module":"deposit","build_sha256":"build","decision":2,"from_thread":1,"runnable_mask":"0x00000007","selected_thread":0,"point_offset":"0x20"},
                {"process":"ledger","module":"deposit","build_sha256":"build","decision":3,"from_thread":0,"runnable_mask":"0x00000001","selected_thread":0,"point_offset":"0x30"}
            ]},
            "serial_sha256": {},
            "state_sha256": "state"
        }))
        .unwrap();
        let mut schedules = vec![source.clone()];
        let mut pending = Vec::new();
        extend_runnable_prefix_schedules(
            &campaign,
            &source,
            &[boundary],
            &mut schedules,
            &mut pending,
        );

        assert_eq!(pending, [1, 2, 3]);
        assert_eq!(schedules[1].thread_schedule_prefixes, [vec![2]]);
        assert_eq!(schedules[2].thread_schedule_prefixes, [vec![1, 1]]);
        assert_eq!(schedules[3].thread_schedule_prefixes, [vec![1, 2]]);
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
    fn campaign_process_lifecycle_excludes_impossible_histories() {
        let campaign: CampaignPlan = serde_json::from_value(serde_json::json!({
            "driver": "api",
            "operations": [
                {
                    "name": "launch",
                    "service": "api",
                    "shell_phase": "launch",
                    "shell_process": "writer",
                    "inputs": [{"name":"default", "input_hex":"00"}],
                    "max_uses": 2
                },
                {
                    "name": "complete",
                    "service": "api",
                    "shell_phase": "completion",
                    "shell_process": "writer",
                    "inputs": [{"name":"default", "input_hex":"00"}],
                    "max_uses": 2
                }
            ],
            "max_runs": 16,
            "max_faults_per_run": 1,
            "max_operations_per_run": 4
        }))
        .unwrap();
        let histories = campaign_operation_histories(&campaign)
            .into_iter()
            .map(|history| {
                history
                    .into_iter()
                    .map(|choice| campaign.operations[choice.operation].name.as_str())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        assert!(histories.contains(&vec!["launch"]));
        assert!(histories.contains(&vec!["launch", "complete"]));
        assert!(histories.contains(&vec!["launch", "complete", "launch"]));
        assert!(!histories.iter().any(|history| history[0] == "complete"));
        assert!(!histories
            .iter()
            .any(|history| history.starts_with(&["launch", "launch"])));
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
