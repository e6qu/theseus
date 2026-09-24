// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Strict Docker Compose-shaped topology input for Theseus guests.
//!
//! Compose is a familiar input for services, image launch settings, networks,
//! and campaigns. Theseus accepts a strict deterministic subset: a service
//! either names an image or a locked Theseus manifest, while host ports and
//! host networks remain unsupported.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use serde_json_path::JsonPath;
use sha2::{Digest, Sha256};

use crate::manifest::{GrpcServingStatus, HttpMethod};
use crate::{load_plan, ArtifactPlan, LoadError, RunPlan};

#[derive(Debug)]
pub enum ComposeError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    Invalid(String),
    Manifest {
        service: String,
        source: Box<LoadError>,
    },
}

impl fmt::Display for ComposeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "cannot parse {}: {source}", path.display())
            }
            Self::Invalid(reason) => write!(formatter, "invalid Compose topology: {reason}"),
            Self::Manifest { service, source } => {
                write!(formatter, "service {service:?}: {source}")
            }
        }
    }
}

impl std::error::Error for ComposeError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeFile {
    #[serde(default)]
    name: Option<String>,
    services: BTreeMap<String, ComposeService>,
    #[serde(default)]
    networks: BTreeMap<String, ComposeNetwork>,
    #[serde(default)]
    configs: BTreeMap<String, ComposeConfigDefinition>,
    #[serde(default)]
    secrets: BTreeMap<String, ComposeConfigDefinition>,
    #[serde(rename = "x-theseus", default)]
    theseus: Option<ComposeTheseus>,
}

/// Topology-wide Theseus configuration.  Keeping campaign input here makes a
/// Compose file the complete description of the system *and* its test
/// campaign; no host-side driver program is required.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeTheseus {
    #[serde(default)]
    replay_start: crate::manifest::ReplayStart,
    #[serde(default)]
    campaign: Option<ComposeCampaign>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeCampaign {
    driver: String,
    /// Expand a bounded catalog of service and directed-network failures from
    /// the locked topology and ordinary operation boundaries.
    #[serde(default)]
    fault_profile: Option<ComposeFaultProfile>,
    /// Discover an Antithesis-compatible test template from the service
    /// images instead of spelling out every command as a Compose operation.
    #[serde(default)]
    test_template: Option<String>,
    /// Restrict image discovery to these templates. When neither this field,
    /// `test_template`, nor explicit operations are present, every discovered
    /// template participates and the explorer selects one per timeline.
    #[serde(default)]
    test_templates: Vec<String>,
    /// Maximum simultaneous copies generated for each discovered parallel or
    /// anytime command. The explorer chooses which slots actually run.
    #[serde(default = "default_test_command_parallelism")]
    max_parallel_commands: u8,
    #[serde(default)]
    guidance: CampaignGuidance,
    #[serde(default)]
    coverage: CampaignCoverage,
    #[serde(default)]
    state: BTreeMap<String, String>,
    #[serde(default)]
    operations: Vec<ComposeOperation>,
    #[serde(default)]
    stages: Vec<String>,
    #[serde(default)]
    faults: Vec<ComposeCampaignFault>,
    /// Quiet windows: at each named operation's barrier every active fault
    /// recovers before the operation executes, except the faults whose own
    /// window closes later.
    #[serde(default)]
    quiet: Vec<ComposeQuietWindow>,
    #[serde(default)]
    properties: Vec<ComposeProperty>,
    /// Named recursive serial-evidence expressions. `use: name` expands one
    /// definition anywhere an evidence expression is accepted.
    #[serde(default)]
    evidence: BTreeMap<String, ComposeSerialEvidence>,
    #[serde(default = "default_campaign_runs")]
    max_runs: u16,
    #[serde(default = "default_campaign_faults_per_run")]
    max_faults_per_run: u8,
    #[serde(default = "default_campaign_operations_per_run")]
    max_operations_per_run: u8,
}

/// One campaign-declared quiet window.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeQuietWindow {
    before: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeOperation {
    name: String,
    /// Populated only by image discovery. This keeps commands from different
    /// template directories out of the same timeline.
    #[serde(skip)]
    test_template: Option<String>,
    /// Populated only by image test-template discovery.
    #[serde(skip)]
    test_command_path: Option<String>,
    /// Optional Test Composer lifecycle role. When one operation declares a
    /// role, every operation in the campaign must declare one.
    #[serde(default)]
    command: Option<ComposeTestCommand>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    input_template: Option<String>,
    #[serde(default)]
    input_captures: BTreeMap<String, ComposeOperationInputCapture>,
    #[serde(default)]
    inputs: Vec<ComposeOperationInput>,
    #[serde(default)]
    input_grammar: Option<ComposeOperationInputGrammar>,
    /// A declared request for an image-backed service. Theseus encodes this
    /// into its pivot protocol, rather than requiring the image to read UART.
    #[serde(default)]
    http: Option<ComposeHttpOperation>,
    /// A standard gRPC health check for an image-backed service. As with HTTP
    /// operations, the pivot owns the protocol; the application sees no UART.
    #[serde(default)]
    grpc_health: Option<ComposeGrpcHealthOperation>,
    /// An argv command run in the image after readiness. This is an ordinary
    /// image workload, not a guest UART protocol.
    #[serde(default)]
    shell: Option<ComposeShellOperation>,
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
    requires_serial: Option<ComposeOperationSerialGuard>,
    #[serde(default)]
    excludes_serial: Option<ComposeOperationSerialGuard>,
    #[serde(default)]
    requires_serial_all: Option<Vec<ComposeOperationSerialGuard>>,
    #[serde(default)]
    excludes_serial_any: Option<Vec<ComposeOperationSerialGuard>>,
    #[serde(default)]
    requires_serial_joins: Option<Vec<ComposeSerialJoin>>,
    #[serde(default)]
    excludes_serial_joins: Option<Vec<ComposeSerialJoin>>,
    #[serde(default)]
    requires_serial_evidence: Option<ComposeSerialEvidence>,
    #[serde(default)]
    excludes_serial_evidence: Option<ComposeSerialEvidence>,
    #[serde(default)]
    max_uses: Option<u8>,
    #[serde(default)]
    requires_state: BTreeMap<String, String>,
    #[serde(default)]
    sets_state: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeHttpOperation {
    #[serde(default)]
    method: HttpMethod,
    url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default = "default_http_status")]
    expect_status: u16,
    #[serde(default)]
    body_contains: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeGrpcHealthOperation {
    url: String,
    #[serde(default)]
    service: String,
    #[serde(default = "default_grpc_status")]
    expect_status: GrpcServingStatus,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeShellOperation {
    #[serde(default)]
    phase: ComposeShellPhase,
    #[serde(default)]
    process: Option<String>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default = "default_shell_exit")]
    expect_exit: i32,
    #[serde(default)]
    output_contains: Option<String>,
    #[serde(default)]
    output_json: bool,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    /// Named runtime choices. A bound of N creates the values 0..N and each
    /// generated input case injects one exact assignment into the command.
    #[serde(default)]
    choices: BTreeMap<String, u16>,
    /// A repeating sequence of stable pthread identities for a command built
    /// with the packaged C scheduling frontend, a bounded search over such
    /// repeating sequences, or feedback-driven runnable-prefix exploration.
    #[serde(default)]
    thread_schedule: ComposeThreadSchedule,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeThreadSchedule {
    Exact(Vec<u8>),
    Search(ComposeThreadScheduleSearch),
    Exploration(ComposeThreadScheduleExploration),
}

impl Default for ComposeThreadSchedule {
    fn default() -> Self {
        Self::Exact(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeThreadScheduleSearch {
    threads: Vec<u8>,
    period: u8,
    max_switches: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeThreadScheduleExploration {
    runnable_prefixes: ComposeRunnablePrefixExploration,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeRunnablePrefixExploration {
    max_choices: u8,
    max_variants: u16,
}

/// Lifecycle step for an ordinary command executed inside an image VM.
/// `launch` deliberately returns before the child exits; a later `completion`
/// step joins that exact named process. The remaining phases execute to
/// completion and exist to make a retained scenario readable.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComposeShellPhase {
    #[default]
    Run,
    Setup,
    Launch,
    Completion,
    Assertion,
    Recovery,
}

/// Scheduling contract for a command in a reusable campaign template. The
/// names intentionally match the established Test Composer vocabulary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComposeTestCommand {
    First,
    ParallelDriver,
    SerialDriver,
    SingletonDriver,
    Anytime,
    Eventually,
    Finally,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeOperationInput {
    name: String,
    input: String,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
    #[serde(default)]
    max_uses: Option<u8>,
    #[serde(default)]
    requires_state: BTreeMap<String, String>,
    #[serde(default)]
    sets_state: BTreeMap<String, String>,
}

/// A finite, declarative input language. Every combination of named choices
/// becomes one ordinary input case in the locked campaign plan.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeOperationInputGrammar {
    template: String,
    #[serde(default)]
    name_template: Option<String>,
    choices: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    input_captures: BTreeMap<String, ComposeOperationInputCapture>,
    #[serde(default)]
    cases: BTreeMap<String, ComposeOperationInputRules>,
}

/// Rules attached to a generated grammar leaf. Keeping this separate from its
/// payload makes the grammar's finite Cartesian product explicit.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ComposeOperationInputRules {
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
    #[serde(default)]
    max_uses: Option<u8>,
    #[serde(default)]
    requires_state: BTreeMap<String, String>,
    #[serde(default)]
    sets_state: BTreeMap<String, String>,
}

/// One value selected from a matching JSON-lines event, local event sequence,
/// or correlated multi-service workflow in a restored campaign checkpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeOperationInputCapture {
    #[serde(default)]
    service: Option<String>,
    pointer: String,
    #[serde(default)]
    json: Option<ComposeJsonPredicate>,
    #[serde(default)]
    sequence: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    workflow: Option<ComposeSerialWorkflow>,
    #[serde(default)]
    encoding: ComposeOperationInputEncoding,
    #[serde(default)]
    select: ComposeOperationInputSelect,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ComposeOperationInputEncoding {
    #[default]
    Text,
    Json,
    Hex,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComposeOperationInputSelect {
    First,
    #[default]
    Latest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeOperationSerialGuard {
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    matches: Option<String>,
    #[serde(default)]
    json: Option<ComposeJsonPredicate>,
    #[serde(default)]
    all: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    any: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    none: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    sequence: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    occurs: Option<ComposeSerialOccurrence>,
}

impl ComposeOperationSerialGuard {
    fn into_predicate(self) -> ComposeSerialPredicate {
        ComposeSerialPredicate {
            contains: self.contains,
            matches: self.matches,
            json: self.json,
            all: self.all,
            any: self.any,
            none: self.none,
            sequence: self.sequence,
            occurs: self.occurs,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeCampaignFault {
    kind: CampaignFaultKind,
    /// Keep this action in every generated schedule that reaches its trigger.
    /// Required actions are also preserved by counterexample minimization.
    #[serde(default)]
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
    /// The barrier where a fault window closes: the fault recovers at this
    /// operation, before it executes, through the same automatic recovery
    /// the terminal lifecycle applies.
    #[serde(default)]
    until: Option<String>,
    /// The argv a `custom` fault runs inside the image at its barrier. It is
    /// passed to execve unchanged, like every shell operation.
    #[serde(default)]
    command: Option<Vec<String>>,
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
    #[serde(default)]
    rate: Option<u32>,
}

/// Campaign-only faults. Lifecycle faults occur on scheduler rounds; topology
/// actions run immediately after a named operation reports its UART barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignFaultKind {
    Pause,
    Restart,
    ClockJump,
    CpuThrottle,
    CpuRelease,
    ClockRate,
    ClockRateRelease,
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
    /// A user-declared argv command run inside an image-backed service at an
    /// operation barrier. It has no automatic inverse and never participates
    /// in terminal recovery.
    Custom,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComposeFaultProfile {
    Standard,
}

fn empty_campaign_fault(kind: CampaignFaultKind) -> ComposeCampaignFault {
    ComposeCampaignFault {
        kind,
        required: false,
        service: None,
        network: None,
        from: None,
        to: None,
        drive: None,
        after: None,
        until: None,
        command: None,
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
        rate: None,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeProperty {
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
    predicate: Option<ComposeSerialPredicate>,
    #[serde(default)]
    requires_serial_all: Option<Vec<ComposeOperationSerialGuard>>,
    #[serde(default)]
    requires_serial_any: Option<Vec<ComposeOperationSerialGuard>>,
    #[serde(default)]
    excludes_serial_any: Option<Vec<ComposeOperationSerialGuard>>,
    #[serde(default)]
    requires_serial_correlations: Option<Vec<ComposeSerialCorrelation>>,
    #[serde(default)]
    requires_serial_joins: Option<Vec<ComposeSerialJoin>>,
    #[serde(default)]
    requires_serial_evidence: Option<ComposeSerialEvidence>,
    #[serde(default)]
    excludes_serial_evidence: Option<ComposeSerialEvidence>,
    #[serde(default)]
    service: Option<String>,
}

/// Require matching JSON Pointer values between two service transcripts.
/// Endpoint services default to the property's service when omitted.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialCorrelation {
    capture: ComposeJsonCorrelationEndpoint,
    equals: ComposeJsonCorrelationEndpoint,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeJsonCorrelationEndpoint {
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    pointer: Option<String>,
    #[serde(default)]
    pointers: Vec<String>,
    json: ComposeJsonPredicate,
}

/// Require one or every JSON value from the first endpoint to occur in every
/// other listed endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialJoin {
    endpoints: Vec<ComposeJsonCorrelationEndpoint>,
    #[serde(default)]
    quantifier: ComposeSerialJoinQuantifier,
    #[serde(default)]
    occurs: Option<ComposeSerialMatchCount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeSerialJoinQuantifier {
    Any,
    Every,
}

impl Default for ComposeSerialJoinQuantifier {
    fn default() -> Self {
        Self::Any
    }
}

/// Require a pair of JSON endpoint values to satisfy one relation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialRelation {
    left: ComposeJsonCorrelationEndpoint,
    right: ComposeJsonCorrelationEndpoint,
    operator: ComposeJsonRelationOperator,
    /// Order the matching events inside one selected serial transcript.
    #[serde(default)]
    order: Option<ComposeSerialRelationOrder>,
    #[serde(default)]
    quantifier: ComposeSerialJoinQuantifier,
    #[serde(default)]
    occurs: Option<ComposeSerialMatchCount>,
}

/// Require one or every key from the first JSON event to complete an ordered
/// path through later events in one service transcript.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialPath {
    #[serde(default)]
    service: Option<String>,
    pointers: Vec<String>,
    steps: Vec<ComposeJsonPredicate>,
    #[serde(default)]
    quantifier: ComposeSerialJoinQuantifier,
    #[serde(default)]
    occurs: Option<ComposeSerialMatchCount>,
}

/// Require one or every key to complete ordered local event paths across
/// several explicitly named services.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialWorkflow {
    pointers: Vec<String>,
    stages: Vec<ComposeSerialWorkflowStage>,
    #[serde(default)]
    quantifier: ComposeSerialJoinQuantifier,
    #[serde(default)]
    occurs: Option<ComposeSerialMatchCount>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialWorkflowStage {
    service: String,
    #[serde(default)]
    pointers: Vec<String>,
    steps: Vec<ComposeJsonPredicate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeSerialRelationOrder {
    Before,
    After,
}

/// Bounds on the number of distinct left/source endpoint values that match.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialMatchCount {
    #[serde(default)]
    exactly: Option<u64>,
    #[serde(default)]
    at_least: Option<u64>,
    #[serde(default)]
    at_most: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeJsonRelationOperator {
    Equals,
    NotEquals,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
}

/// A recursive boolean expression over service-scoped serial predicates,
/// correlations, JSON joins, and value relations. Exactly one member is
/// allowed at each node.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialEvidence {
    #[serde(default)]
    all: Vec<ComposeSerialEvidence>,
    #[serde(default)]
    any: Vec<ComposeSerialEvidence>,
    #[serde(default)]
    none: Vec<ComposeSerialEvidence>,
    #[serde(default)]
    guard: Option<ComposeOperationSerialGuard>,
    #[serde(default)]
    correlation: Option<ComposeSerialCorrelation>,
    #[serde(default)]
    join: Option<ComposeSerialJoin>,
    #[serde(default)]
    relation: Option<ComposeSerialRelation>,
    #[serde(default)]
    path: Option<ComposeSerialPath>,
    #[serde(default)]
    workflow: Option<ComposeSerialWorkflow>,
    #[serde(default, rename = "use")]
    use_: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialPredicate {
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    matches: Option<String>,
    #[serde(default)]
    json: Option<ComposeJsonPredicate>,
    #[serde(default)]
    all: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    any: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    none: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    sequence: Vec<ComposeSerialPredicate>,
    #[serde(default)]
    occurs: Option<ComposeSerialOccurrence>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeSerialOccurrence {
    predicate: Box<ComposeSerialPredicate>,
    #[serde(default)]
    exactly: Option<u64>,
    #[serde(default)]
    at_least: Option<u64>,
    #[serde(default)]
    at_most: Option<u64>,
}

/// One JSON-lines event emitted on the serial console. Every pointer/value pair
/// must match the same JSON object, so related fields cannot be satisfied by
/// separate log lines.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeJsonPredicate {
    /// An RFC 9535 JSONPath query that must select at least one node from this
    /// same JSON-lines event.
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    fields: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "where")]
    where_: Vec<ComposeJsonCondition>,
    #[serde(default)]
    arrays: Vec<ComposeJsonArrayPredicate>,
    /// Require every nested predicate to match this same JSON event.
    #[serde(default)]
    all: Vec<ComposeJsonPredicate>,
    /// Require at least one nested predicate to match this same JSON event.
    #[serde(default)]
    any: Vec<ComposeJsonPredicate>,
    /// Require no nested predicate to match this same JSON event.
    #[serde(default)]
    none: Vec<ComposeJsonPredicate>,
    /// Bind a value from this JSON-lines event for a later item in the same
    /// ordered serial sequence. Keys are capture names, values are pointers.
    #[serde(default)]
    capture: BTreeMap<String, String>,
    /// Require values on this event to equal values captured by an earlier
    /// JSON-lines item. Keys are pointers, values are capture names.
    #[serde(default)]
    equals_capture: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeJsonArrayPredicate {
    pointer: String,
    #[serde(default)]
    any: Option<Box<ComposeJsonPredicate>>,
    #[serde(default)]
    all: Option<Box<ComposeJsonPredicate>>,
    #[serde(default)]
    none: Option<Box<ComposeJsonPredicate>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeJsonCondition {
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
pub enum PropertyKind {
    /// Every generated timeline must report the property.
    Always,
    /// Every generated timeline must report the property, or none may reach
    /// it at all; a corpus where only some timelines report it fails.
    AlwaysOrUnreachable,
    /// At least one generated timeline must report the property.
    Sometimes,
    /// The campaign must reach a timeline that reports the property.
    Reachable,
    /// No generated timeline may report the property.
    Unreachable,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeNetwork {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeService {
    #[serde(rename = "x-theseus")]
    theseus: ServiceTheseus,
    #[serde(default)]
    networks: Vec<String>,
    #[serde(default)]
    depends_on: Option<ComposeDependencies>,
    #[serde(default)]
    environment: Option<ComposeEnvironment>,
    #[serde(default)]
    env_file: Option<ComposeEnvFiles>,
    /// The Compose launch fields are intentionally an argv-only subset. A
    /// shell string would add image-specific parsing rules to the locked
    /// topology, whereas an argv is the exact execve contract.
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    configs: Vec<ComposeServiceConfig>,
    #[serde(default)]
    secrets: Vec<ComposeServiceConfig>,
    #[serde(default)]
    volumes: Vec<ComposeServiceVolume>,
    #[serde(default)]
    healthcheck: Option<ComposeHealthcheck>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    extra_hosts: Option<ComposeExtraHosts>,
    #[serde(default)]
    cpus: Option<ComposeQuantity>,
    #[serde(default)]
    mem_limit: Option<ComposeQuantity>,
    #[serde(default)]
    deploy: Option<ComposeDeploy>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    tmpfs: Vec<String>,
}

/// Compose accepts quantities as either YAML numbers or strings. Theseus
/// normalizes the small deterministic subset it can express as VM resources.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ComposeQuantity {
    Text(String),
    Integer(u64),
    Decimal(f64),
}

impl ComposeQuantity {
    fn literal(&self) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::Integer(value) => value.to_string(),
            Self::Decimal(value) => value.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeDeploy {
    #[serde(default)]
    resources: Option<ComposeDeployResources>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeDeployResources {
    #[serde(default)]
    limits: Option<ComposeResourceLimits>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeResourceLimits {
    #[serde(default)]
    cpus: Option<ComposeQuantity>,
    #[serde(default)]
    memory: Option<ComposeQuantity>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeConfigDefinition {
    file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeServiceConfig {
    Name(String),
    Mount(ComposeConfigMount),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeConfigMount {
    source: String,
    #[serde(default)]
    target: Option<String>,
}

/// A constrained Compose bind mount. Theseus locks a local directory into the
/// image initramfs instead of keeping a host mount alive at runtime.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeServiceVolume {
    Short(String),
    Mount(ComposeVolumeMount),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeVolumeMount {
    #[serde(rename = "type")]
    kind: String,
    source: String,
    target: String,
    #[serde(default)]
    read_only: bool,
}

/// The argv-only Compose health-check subset. Shell health checks would add
/// image-specific parsing rules to a replay, so Theseus accepts `CMD` only.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeHealthcheck {
    test: ComposeHealthcheckTest,
    #[serde(default)]
    interval: Option<String>,
    #[serde(default)]
    retries: Option<u32>,
    #[serde(default)]
    start_period: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeHealthcheckTest {
    Command(Vec<String>),
    Disabled(String),
}

/// Literal Compose environment values. Host-environment inheritance is
/// deliberately excluded: it would make a locked topology depend on the
/// machine that happened to create it.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeEnvironment {
    Map(BTreeMap<String, String>),
    List(Vec<String>),
}

/// Local Compose environment files. Theseus reads them while planning and
/// stores their literal values, never consulting the host environment later.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeEnvFiles {
    Single(String),
    List(Vec<String>),
}

/// Compose's local host aliases. Theseus accepts either the mapping form or
/// the portable short form (`name=address` or `name:address`) and locks the
/// resolved IP address into the image initramfs.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeExtraHosts {
    Map(BTreeMap<String, String>),
    List(Vec<String>),
}

/// Compose accepts either a short dependency list or a map carrying one
/// condition per service. Theseus intentionally implements the two startup
/// conditions it can prove from its deterministic boot barrier.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ComposeDependencies {
    Names(Vec<String>),
    Conditions(BTreeMap<String, ComposeDependency>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeDependency {
    #[serde(default)]
    condition: DependencyCondition,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyCondition {
    #[default]
    ServiceStarted,
    ServiceHealthy,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceTheseus {
    manifest: PathBuf,
    #[serde(default)]
    faults: Vec<ComposeFault>,
    #[serde(default)]
    coverage: Vec<ComposeCoverage>,
}

/// One compiler-generated coverage manifest and the directory containing the
/// build-scoped symbol file named by that manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeCoverage {
    manifest: PathBuf,
    symbols: PathBuf,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeFault {
    at_round: u64,
    kind: FaultKind,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    Pause,
    Restart,
    ClockJump,
}

#[derive(Debug, Clone, Serialize)]
pub struct FaultPlan {
    pub at_round: u64,
    pub kind: FaultKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_rounds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nanoseconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComposePlan {
    pub format: String,
    pub compose: String,
    pub name: Option<String>,
    pub services: BTreeMap<String, ComposeServicePlan>,
    /// Network name to sorted service names. Every member can exchange frames
    /// once the deterministic multi-guest switch is available.
    pub networks: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign: Option<CampaignPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topology_runner: Option<ArtifactPlan>,
    pub replay_start: crate::manifest::ReplayStart,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComposeServicePlan {
    pub manifest: String,
    pub run: RunPlan,
    pub networks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<DependencyPlan>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<ImageLaunchPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub configs: Vec<ImageConfigPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ImageConfigPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<ImageVolumePlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<ImageHealthcheckPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_hosts: BTreeMap<String, String>,
    pub faults: Vec<FaultPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<CoverageArtifactPlan>,
}

/// Immutable coverage metadata and symbols consumed by campaign reporting.
#[derive(Debug, Clone, Serialize)]
pub struct CoverageArtifactPlan {
    pub format: String,
    pub coverage: String,
    pub language: String,
    pub process: String,
    pub module: String,
    pub build_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gnu_build_id: Option<String>,
    pub manifest: ArtifactPlan,
    pub symbols: ArtifactPlan,
}

/// A read-only Compose config baked into an image-backed service initramfs.
#[derive(Debug, Clone, Serialize)]
pub struct ImageConfigPlan {
    pub target: String,
    pub data: Vec<u8>,
}

/// A writable image directory seeded from a local Compose bind source.
#[derive(Debug, Clone, Serialize)]
pub struct ImageVolumePlan {
    pub target: String,
    pub directories: Vec<String>,
    pub files: Vec<ImageConfigPlan>,
}

/// A standard Compose `CMD` health check, locked for the injected image pivot.
#[derive(Debug, Clone, Serialize)]
pub struct ImageHealthcheckPlan {
    pub command: Vec<String>,
    pub interval_millis: u64,
    pub retries: u32,
    pub start_period_millis: u64,
}

/// Literal image launch overrides from a Compose service. They become part of
/// the derived initramfs contract, never a host-side command.
#[derive(Debug, Clone, Serialize)]
pub struct ImageLaunchPlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<ImageUserPlan>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmpfs: Vec<String>,
}

/// Numeric credentials for a Compose image process. Name lookup would make a
/// plan depend on image-specific account files, so Theseus locks uid:gid.
#[derive(Debug, Clone, Serialize)]
pub struct ImageUserPlan {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyPlan {
    pub service: String,
    pub condition: DependencyCondition,
}

/// A deterministic, serial-driven topology campaign.  Operations are UTF-8
/// UART input for the designated workload service.  The same line protocol is
/// usable from a shell or C program; an SDK is optional.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CampaignGuidance {
    Coverage,
    Adaptive,
    Posterior,
    Property,
    /// One decision-tree policy across inputs, faults, schedules, coverage,
    /// topology state, and property evidence.
    #[default]
    Unified,
}

/// Choose the primary deterministic signal used to rank campaign schedules.
/// Application blocks and edges are explicit records emitted by an
/// instrumented process; the other modes are lower-fidelity baselines
/// collected by the runtime.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CampaignCoverage {
    Markers,
    CheckpointPcs,
    #[default]
    ExecutionLocations,
    ApplicationBlocks,
    ApplicationEdges,
}

fn is_default_campaign_coverage(value: &CampaignCoverage) -> bool {
    *value == CampaignCoverage::ExecutionLocations
}

/// One campaign-declared quiet window.
#[derive(Debug, Clone, Serialize)]
pub struct QuietWindowPlan {
    pub before: String,
}

/// One locked shard of the candidate corpus: `index` out of `total`
/// parallel workers, so the workers cover disjoint, deterministic
/// partitions of the same corpus.
#[derive(Debug, Clone, Serialize)]
pub struct ShardPlan {
    pub index: u16,
    pub total: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct CampaignPlan {
    pub driver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault_profile: Option<ComposeFaultProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_template: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_templates: Vec<String>,
    #[serde(default = "default_test_command_parallelism")]
    pub max_parallel_commands: u8,
    #[serde(default)]
    pub guidance: CampaignGuidance,
    #[serde(default, skip_serializing_if = "is_default_campaign_coverage")]
    pub coverage: CampaignCoverage,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state: BTreeMap<String, String>,
    pub operations: Vec<OperationPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<String>,
    pub faults: Vec<CampaignFaultPlan>,
    pub properties: Vec<PropertyPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quiet: Vec<QuietWindowPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<ShardPlan>,
    pub max_runs: u16,
    pub max_faults_per_run: u8,
    pub max_operations_per_run: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationPlan {
    pub name: String,
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<ComposeTestCommand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_command_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_phase: Option<ComposeShellPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_process: Option<String>,
    /// The argv this shell operation runs, retained so the standard fault
    /// profile can propose custom candidates from a service's own commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread_schedule: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_schedule_search: Option<ThreadScheduleSearchPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_schedule_exploration: Option<ThreadScheduleExplorationPlan>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub choice_bounds: BTreeMap<String, u16>,
    pub inputs: Vec<OperationInputPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_grammar: Option<OperationInputGrammarPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_markers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes_markers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_serial: Option<OperationSerialGuardPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excludes_serial: Option<OperationSerialGuardPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_serial_all: Vec<OperationSerialGuardPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes_serial_any: Vec<OperationSerialGuardPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_serial_joins: Vec<SerialJoinPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes_serial_joins: Vec<SerialJoinPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_serial_evidence: Option<SerialEvidencePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excludes_serial_evidence: Option<SerialEvidencePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u8>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requires_state: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sets_state: BTreeMap<String, String>,
}

/// Source-level description retained in locked plans and reports. The runner
/// executes the expanded `inputs`, so replay never depends on re-expansion.
#[derive(Debug, Clone, Serialize)]
pub struct OperationInputGrammarPlan {
    pub template: String,
    pub name_template: String,
    pub choices: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_captures: BTreeMap<String, OperationInputCapturePlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationInputPlan {
    pub name: String,
    pub input_hex: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub choices: BTreeMap<String, u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread_schedule: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_template: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_captures: BTreeMap<String, OperationInputCapturePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<OperationInputReferencePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<OperationInputReferencePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u8>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requires_state: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sets_state: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadScheduleSearchPlan {
    pub threads: Vec<u8>,
    pub period: u8,
    pub max_switches: u8,
    pub generated_schedules: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadScheduleExplorationPlan {
    pub strategy: &'static str,
    pub max_choices: u8,
    pub max_variants: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationInputCapturePlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub pointer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json: Option<JsonPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sequence: Vec<SerialPredicatePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<SerialWorkflowPlan>,
    pub encoding: ComposeOperationInputEncoding,
    pub select: ComposeOperationInputSelect,
}

/// A case transition can name any logical operation (`write`) or one exact
/// payload case (`write[retry]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationInputReferencePlan {
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationSerialGuardPlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(flatten)]
    pub predicate: SerialPredicatePlan,
}

#[derive(Debug, Clone, Serialize)]
pub struct CampaignFaultPlan {
    pub kind: CampaignFaultKind,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_input: Option<OperationInputReferencePlan>,
    /// The barrier where a fault window closes, through the automatic
    /// recovery the terminal lifecycle applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// The argv a `custom` fault runs inside its image-backed service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_round: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_rounds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nanoseconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torn_write_bytes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrupt_read_xor: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ethertype: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_protocol: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drop_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrupt_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_bytes_per_round: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu_bytes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_queue_frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_queue_frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub every_n_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PropertyPlan {
    pub name: String,
    pub kind: PropertyKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains_all: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains_none: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicate: Option<SerialPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_serial_all: Vec<OperationSerialGuardPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_serial_any: Vec<OperationSerialGuardPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes_serial_any: Vec<OperationSerialGuardPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_serial_correlations: Vec<SerialCorrelationPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_serial_joins: Vec<SerialJoinPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_serial_evidence: Option<SerialEvidencePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excludes_serial_evidence: Option<SerialEvidencePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialCorrelationPlan {
    pub capture: JsonCorrelationEndpointPlan,
    pub equals: JsonCorrelationEndpointPlan,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonCorrelationEndpointPlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub pointers: Vec<String>,
    pub json: JsonPredicatePlan,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialJoinPlan {
    pub endpoints: Vec<JsonCorrelationEndpointPlan>,
    pub quantifier: ComposeSerialJoinQuantifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurs: Option<SerialMatchCountPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialRelationPlan {
    pub left: JsonCorrelationEndpointPlan,
    pub right: JsonCorrelationEndpointPlan,
    pub operator: ComposeJsonRelationOperator,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<ComposeSerialRelationOrder>,
    pub quantifier: ComposeSerialJoinQuantifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurs: Option<SerialMatchCountPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialPathPlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub pointers: Vec<String>,
    pub steps: Vec<JsonPredicatePlan>,
    pub quantifier: ComposeSerialJoinQuantifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurs: Option<SerialMatchCountPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialWorkflowPlan {
    pub pointers: Vec<String>,
    pub stages: Vec<SerialWorkflowStagePlan>,
    pub quantifier: ComposeSerialJoinQuantifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurs: Option<SerialMatchCountPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialWorkflowStagePlan {
    pub service: String,
    pub pointers: Vec<String>,
    pub steps: Vec<JsonPredicatePlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialMatchCountPlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exactly: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_least: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_most: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialEvidencePlan {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all: Vec<SerialEvidencePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any: Vec<SerialEvidencePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub none: Vec<SerialEvidencePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guard: Option<OperationSerialGuardPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation: Option<SerialCorrelationPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join: Option<SerialJoinPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<SerialRelationPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<SerialPathPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<SerialWorkflowPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialPredicatePlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json: Option<JsonPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all: Vec<SerialPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any: Vec<SerialPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub none: Vec<SerialPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sequence: Vec<SerialPredicatePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurs: Option<SerialOccurrencePlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialOccurrencePlan {
    pub predicate: Box<SerialPredicatePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exactly: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_least: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_most: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonPredicatePlan {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub fields: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "where")]
    pub where_: Vec<JsonConditionPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arrays: Vec<JsonArrayPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all: Vec<JsonPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any: Vec<JsonPredicatePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub none: Vec<JsonPredicatePlan>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capture: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub equals_capture: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonArrayPredicatePlan {
    pub pointer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub any: Option<Box<JsonPredicatePlan>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all: Option<Box<JsonPredicatePlan>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub none: Option<Box<JsonPredicatePlan>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonConditionPlan {
    pub pointer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equals: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub greater_than: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub greater_than_or_equal: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub less_than: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub less_than_or_equal: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
}

/// Load a Compose topology and lock every referenced service artifact into a
/// normalized plan. Relative paths are rooted at the Compose file and may not
/// escape its directory.
pub fn load_compose_plan(path: impl AsRef<Path>) -> Result<ComposePlan, ComposeError> {
    let path = path.as_ref();
    let compose_path = fs::canonicalize(path).map_err(|source| ComposeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let compose_dir = compose_path.parent().expect("canonical path has a parent");
    let input = fs::read_to_string(&compose_path).map_err(|source| ComposeError::Read {
        path: compose_path.clone(),
        source,
    })?;
    let compose: ComposeFile =
        serde_yaml::from_str(&input).map_err(|source| ComposeError::Parse {
            path: compose_path.clone(),
            source,
        })?;

    if compose.services.is_empty() {
        return Err(ComposeError::Invalid(
            "services must not be empty".to_owned(),
        ));
    }
    if compose.networks.is_empty() {
        return Err(ComposeError::Invalid(
            "declare at least one named network; Theseus never uses a host default network"
                .to_owned(),
        ));
    }

    let mut memberships: BTreeMap<String, BTreeSet<String>> = compose
        .networks
        .keys()
        .map(|name| (name.clone(), BTreeSet::new()))
        .collect();
    let configs = load_compose_files(compose_dir, compose.configs, "config")?;
    let secrets = load_compose_files(compose_dir, compose.secrets, "secret")?;
    let mut services = BTreeMap::new();
    for (name, service) in compose.services {
        validate_name("service", &name)?;
        let depends_on = dependency_plan(&name, service.depends_on)?;
        let environment =
            environment_plan(&name, compose_dir, service.env_file, service.environment)?;
        let launch = image_launch_plan(
            &name,
            service.command,
            service.entrypoint,
            service.working_dir,
            service.user,
            service.read_only,
            service.tmpfs,
        )?;
        let configs = image_config_plan(&name, service.configs, &configs)?;
        let secrets = image_secret_plan(&name, service.secrets, &secrets)?;
        let volumes = image_volume_plan(&name, service.volumes, compose_dir)?;
        let healthcheck = image_healthcheck_plan(&name, service.healthcheck)?;
        let (hostname, extra_hosts) =
            host_identity_plan(&name, service.hostname, service.extra_hosts)?;
        if service.networks.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} must join at least one named network"
            )));
        }
        let mut networks = BTreeSet::new();
        for network in service.networks {
            validate_name("network", &network)?;
            let Some(members) = memberships.get_mut(&network) else {
                return Err(ComposeError::Invalid(format!(
                    "service {name:?} references undeclared network {network:?}"
                )));
            };
            members.insert(name.clone());
            networks.insert(network);
        }
        if service.theseus.manifest.is_absolute() {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} x-theseus.manifest must be relative to the Compose file"
            )));
        }
        let manifest =
            fs::canonicalize(compose_dir.join(&service.theseus.manifest)).map_err(|source| {
                ComposeError::Read {
                    path: compose_dir.join(&service.theseus.manifest),
                    source,
                }
            })?;
        if !manifest.starts_with(compose_dir) {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} x-theseus.manifest must not escape the Compose directory"
            )));
        }
        let mut run = load_plan(&manifest).map_err(|source| ComposeError::Manifest {
            service: name.clone(),
            source: Box::new(source),
        })?;
        if run.run.replay_start != crate::manifest::ReplayStart::FreshBoot
            || run.checkpoint.is_some()
        {
            return Err(ComposeError::Invalid(format!(
                "service {name:?}: ready_checkpoint is a single-service `theseus test` workflow; Compose manages topology checkpoints"
            )));
        }
        apply_resource_limits(
            &name,
            &mut run,
            service.cpus,
            service.mem_limit,
            service
                .deploy
                .and_then(|deploy| deploy.resources)
                .and_then(|resources| resources.limits),
        )?;
        if (launch.is_some()
            || !configs.is_empty()
            || !secrets.is_empty()
            || !volumes.is_empty()
            || healthcheck.is_some()
            || hostname.is_some()
            || !extra_hosts.is_empty())
            && run.guest.image.is_none()
        {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} uses an image launch, config, secret, volume, healthcheck, or host-identity contract but its manifest has no guest.image"
            )));
        }
        let faults = validate_faults(
            &name,
            service.theseus.faults,
            run.run.virtual_time.is_some(),
        )?;
        let coverage = coverage_artifact_plans(&name, service.theseus.coverage, compose_dir)?;
        services.insert(
            name,
            ComposeServicePlan {
                manifest: manifest.display().to_string(),
                run,
                networks: networks.into_iter().collect(),
                depends_on,
                environment,
                launch,
                configs,
                secrets,
                volumes,
                healthcheck,
                hostname,
                extra_hosts,
                faults,
                coverage,
            },
        );
    }

    validate_dependency_graph(&services)?;

    let replay_start = compose
        .theseus
        .as_ref()
        .map_or(crate::manifest::ReplayStart::FreshBoot, |theseus| {
            theseus.replay_start
        });
    if replay_start == crate::manifest::ReplayStart::ReadyCheckpoint
        && services
            .values()
            .any(|service| service.run.run.virtual_time.is_none())
    {
        return Err(ComposeError::Invalid(
            "ready_checkpoint requires virtual time on every service".to_owned(),
        ));
    }
    let campaign = campaign_plan(compose.theseus, &mut services)?;
    let networks = memberships
        .into_iter()
        .map(|(name, services)| (name, services.into_iter().collect()))
        .collect();
    Ok(ComposePlan {
        format: "theseus-compose-plan-v1".to_owned(),
        compose: compose_path.display().to_string(),
        name: compose.name,
        services,
        networks,
        campaign,
        topology_runner: None,
        replay_start,
    })
}

fn coverage_artifact_plans(
    service: &str,
    entries: Vec<ComposeCoverage>,
    compose_dir: &Path,
) -> Result<Vec<CoverageArtifactPlan>, ComposeError> {
    if entries.len() > 128 {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} declares more than 128 coverage modules"
        )));
    }
    let mut identities = BTreeSet::new();
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        let manifest_path = compose_coverage_path(
            service,
            "coverage manifest",
            compose_dir,
            &entry.manifest,
            false,
        )?;
        let symbols_dir = compose_coverage_path(
            service,
            "coverage symbols",
            compose_dir,
            &entry.symbols,
            true,
        )?;
        let manifest_bytes = fs::read(&manifest_path).map_err(|source| ComposeError::Read {
            path: manifest_path.clone(),
            source,
        })?;
        let manifest: CoverageManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|error| {
                ComposeError::Invalid(format!(
                    "service {service:?} coverage manifest {} is invalid JSON: {error}",
                    entry.manifest.display()
                ))
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
                | ("theseus-java-coverage-build-v1", "classes", "java")
        );
        if !supported {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} coverage manifest {} has an unsupported format, coverage kind, or language",
                entry.manifest.display()
            )));
        }
        validate_coverage_identity(service, "process", &manifest.process)?;
        validate_coverage_identity(service, "module", &manifest.module)?;
        if manifest.build_sha256.len() != 64
            || !manifest
                .build_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} coverage manifest {} has an invalid build SHA-256",
                entry.manifest.display()
            )));
        }
        if let Some(build_id) = &manifest.gnu_build_id {
            if build_id.is_empty()
                || build_id.len() > 128
                || build_id.len() % 2 != 0
                || !build_id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(ComposeError::Invalid(format!(
                    "service {service:?} coverage manifest {} has an invalid GNU build ID",
                    entry.manifest.display()
                )));
            }
        }
        let symbol_name = Path::new(&manifest.symbols);
        let mut components = symbol_name.components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} coverage manifest {} must name one symbol file",
                entry.manifest.display()
            )));
        }
        let symbol_path = fs::canonicalize(symbols_dir.join(symbol_name)).map_err(|source| {
            ComposeError::Read {
                path: symbols_dir.join(symbol_name),
                source,
            }
        })?;
        if !symbol_path.starts_with(&symbols_dir) || !symbol_path.is_file() {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} coverage symbol {} must be a regular file in {}",
                manifest.symbols,
                entry.symbols.display()
            )));
        }
        let symbol_bytes = fs::read(&symbol_path).map_err(|source| ComposeError::Read {
            path: symbol_path.clone(),
            source,
        })?;
        if !symbol_bytes
            .windows(manifest.build_sha256.len())
            .any(|window| window == manifest.build_sha256.as_bytes())
        {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} coverage symbol {} does not match build {}",
                manifest.symbols, manifest.build_sha256
            )));
        }
        if manifest.format == "theseus-java-coverage-build-v1" {
            // The Java symbol artifact is the frontend's class-to-offset
            // map, not an ELF object: validate the map itself.
            let map: serde_json::Value =
                serde_json::from_slice(&symbol_bytes).map_err(|error| {
                    ComposeError::Invalid(format!(
                        "service {service:?} coverage symbol {} is not a Java symbol map: {error}",
                        manifest.symbols
                    ))
                })?;
            if map["format"] != "theseus-java-coverage-symbols-v1"
                || map["build_sha256"] != manifest.build_sha256
                || map["classes"]
                    .as_array()
                    .is_none_or(|classes| classes.is_empty())
            {
                return Err(ComposeError::Invalid(format!(
                    "service {service:?} coverage symbol {} does not describe build {}",
                    manifest.symbols, manifest.build_sha256
                )));
            }
        } else if !symbol_bytes.starts_with(b"\x7fELF") {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} coverage symbol {} is not an ELF object",
                manifest.symbols
            )));
        }
        let identity = (
            manifest.process.clone(),
            manifest.module.clone(),
            manifest.build_sha256.clone(),
        );
        if !identities.insert(identity) {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} declares the same coverage build more than once"
            )));
        }
        result.push(CoverageArtifactPlan {
            format: manifest.format,
            coverage: manifest.coverage,
            language: manifest.language,
            process: manifest.process,
            module: manifest.module,
            build_sha256: manifest.build_sha256,
            gnu_build_id: manifest.gnu_build_id,
            manifest: artifact_for_file(&manifest_path)?,
            symbols: artifact_for_file(&symbol_path)?,
        });
    }
    result.sort_by(|left, right| {
        left.process
            .cmp(&right.process)
            .then_with(|| left.module.cmp(&right.module))
            .then_with(|| left.build_sha256.cmp(&right.build_sha256))
    });
    Ok(result)
}

fn compose_coverage_path(
    service: &str,
    field: &str,
    compose_dir: &Path,
    relative: &Path,
    directory: bool,
) -> Result<PathBuf, ComposeError> {
    if relative.is_absolute() {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} {field} must be relative to the Compose file"
        )));
    }
    let path =
        fs::canonicalize(compose_dir.join(relative)).map_err(|source| ComposeError::Read {
            path: compose_dir.join(relative),
            source,
        })?;
    if !path.starts_with(compose_dir)
        || (directory && !path.is_dir())
        || (!directory && !path.is_file())
    {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} {field} must {} inside the Compose directory",
            if directory {
                "be a directory"
            } else {
                "name a regular file"
            }
        )));
    }
    Ok(path)
}

fn validate_coverage_identity(service: &str, field: &str, value: &str) -> Result<(), ComposeError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} coverage {field} {value:?} is invalid"
        )));
    }
    Ok(())
}

fn artifact_for_file(path: &Path) -> Result<ArtifactPlan, ComposeError> {
    let bytes = fs::read(path).map_err(|source| ComposeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(ArtifactPlan {
        path: path.display().to_string(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
    })
}

fn image_secret_plan(
    service: &str,
    mounts: Vec<ComposeServiceConfig>,
    definitions: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<ImageConfigPlan>, ComposeError> {
    let mut targets = BTreeSet::new();
    let mut result = Vec::new();
    for mount in mounts {
        let (source, target) = match mount {
            ComposeServiceConfig::Name(source) => {
                (source.clone(), format!("/run/secrets/{source}"))
            }
            ComposeServiceConfig::Mount(mount) => {
                let target = mount
                    .target
                    .unwrap_or_else(|| format!("/run/secrets/{}", mount.source));
                (mount.source, target)
            }
        };
        let Some(data) = definitions.get(&source) else {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} references unknown secret {source:?}"
            )));
        };
        let path = Path::new(&target);
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            || target.contains('\0')
        {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} secret target {target:?} must be an absolute path without parent traversal"
            )));
        }
        if !targets.insert(target.clone()) {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} mounts more than one secret at {target:?}"
            )));
        }
        result.push(ImageConfigPlan {
            target,
            data: data.clone(),
        });
    }
    Ok(result)
}

fn image_volume_plan(
    service: &str,
    mounts: Vec<ComposeServiceVolume>,
    compose_dir: &Path,
) -> Result<Vec<ImageVolumePlan>, ComposeError> {
    let mut targets = BTreeSet::new();
    let mut result = Vec::new();
    for mount in mounts {
        let (source, target) = match mount {
            ComposeServiceVolume::Short(mount) => {
                let Some((source, target)) = mount.split_once(':') else {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} volume {mount:?} must be ./source:/absolute/target"
                    )));
                };
                if target.contains(':') {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} volume {mount:?} must not include a mode"
                    )));
                }
                (source.to_owned(), target.to_owned())
            }
            ComposeServiceVolume::Mount(mount) => {
                if mount.kind != "bind" {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} volume type must be bind"
                    )));
                }
                if mount.read_only {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} read-only volumes are not supported; use configs or secrets"
                    )));
                }
                (mount.source, mount.target)
            }
        };
        if !source.starts_with("./") {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} volume source {source:?} must start with ./"
            )));
        }
        let target_path = Path::new(&target);
        if target == "/"
            || !target_path.is_absolute()
            || target_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            || target.contains('\0')
        {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} volume target {target:?} must be a non-root absolute path without parent traversal"
            )));
        }
        if !targets.insert(target.clone()) {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} mounts more than one volume at {target:?}"
            )));
        }
        let source_path = fs::canonicalize(compose_dir.join(&source)).map_err(|source_error| {
            ComposeError::Read {
                path: compose_dir.join(&source),
                source: source_error,
            }
        })?;
        if !source_path.starts_with(compose_dir) {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} volume source {source:?} must not escape the Compose directory"
            )));
        }
        if !source_path.is_dir() {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} volume source {source:?} must be a directory"
            )));
        }
        result.push(read_volume_tree(service, &source_path, target)?);
    }
    Ok(result)
}

fn image_healthcheck_plan(
    service: &str,
    healthcheck: Option<ComposeHealthcheck>,
) -> Result<Option<ImageHealthcheckPlan>, ComposeError> {
    let Some(healthcheck) = healthcheck else {
        return Ok(None);
    };
    let command = match healthcheck.test {
        ComposeHealthcheckTest::Command(mut test) => {
            if test.first().is_some_and(|entry| entry == "CMD") {
                test.remove(0);
                test
            } else {
                return Err(ComposeError::Invalid(format!(
                    "service {service:?} healthcheck.test must start with CMD; CMD-SHELL is not deterministic"
                )));
            }
        }
        ComposeHealthcheckTest::Disabled(value) if value == "NONE" => return Ok(None),
        ComposeHealthcheckTest::Disabled(value) => {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} healthcheck.test {value:?} must be a CMD argv or NONE"
            )));
        }
    };
    if command.is_empty()
        || command
            .iter()
            .any(|entry| entry.is_empty() || entry.contains('\0'))
    {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} healthcheck CMD must contain non-empty arguments without NUL bytes"
        )));
    }
    let interval_millis = healthcheck
        .interval
        .as_deref()
        .map(|value| parse_compose_duration(service, "healthcheck.interval", value))
        .transpose()?
        .unwrap_or(30_000);
    let start_period_millis = healthcheck
        .start_period
        .as_deref()
        .map(|value| parse_compose_duration(service, "healthcheck.start_period", value))
        .transpose()?
        .unwrap_or(0);
    let retries = healthcheck.retries.unwrap_or(3);
    if retries == 0 {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} healthcheck.retries must be at least 1"
        )));
    }
    Ok(Some(ImageHealthcheckPlan {
        command,
        interval_millis,
        retries,
        start_period_millis,
    }))
}

fn parse_compose_duration(service: &str, field: &str, value: &str) -> Result<u64, ComposeError> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} {field} {value:?} must use ms, s, or m"
        )));
    };
    let millis = number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier));
    match millis.filter(|millis| *millis > 0) {
        Some(millis) => Ok(millis),
        None => Err(ComposeError::Invalid(format!(
            "service {service:?} {field} {value:?} must be a positive duration"
        ))),
    }
}

fn read_volume_tree(
    service: &str,
    source: &Path,
    target: String,
) -> Result<ImageVolumePlan, ComposeError> {
    let mut directories = BTreeSet::from([target.clone()]);
    let mut files = Vec::new();
    let mut pending = vec![(source.to_path_buf(), PathBuf::new())];
    while let Some((directory, relative)) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .map_err(|source_error| ComposeError::Read {
                path: directory.clone(),
                source: source_error,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source_error| ComposeError::Read {
                path: directory.clone(),
                source: source_error,
            })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
            let name = entry.file_name();
            let relative = relative.join(&name);
            let destination = Path::new(&target).join(&relative);
            let destination = destination.to_str().ok_or_else(|| {
                ComposeError::Invalid(format!(
                    "service {service:?} volume contains a non-UTF-8 path"
                ))
            })?;
            let file_type = entry
                .file_type()
                .map_err(|source_error| ComposeError::Read {
                    path: entry.path(),
                    source: source_error,
                })?;
            if file_type.is_dir() {
                directories.insert(destination.to_owned());
                pending.push((entry.path(), relative));
            } else if file_type.is_file() {
                let path = entry.path();
                let data = fs::read(&path).map_err(|source_error| ComposeError::Read {
                    path,
                    source: source_error,
                })?;
                files.push(ImageConfigPlan {
                    target: destination.to_owned(),
                    data,
                });
            } else {
                return Err(ComposeError::Invalid(format!(
                    "service {service:?} volume source contains an unsupported non-regular file"
                )));
            }
        }
    }
    files.sort_by(|left, right| left.target.cmp(&right.target));
    Ok(ImageVolumePlan {
        target,
        directories: directories.into_iter().collect(),
        files,
    })
}

fn load_compose_files(
    compose_dir: &Path,
    definitions: BTreeMap<String, ComposeConfigDefinition>,
    kind: &str,
) -> Result<BTreeMap<String, Vec<u8>>, ComposeError> {
    let mut configs = BTreeMap::new();
    for (name, definition) in definitions {
        validate_name(kind, &name)?;
        if definition.file.is_absolute() {
            return Err(ComposeError::Invalid(format!(
                "{kind} {name:?} file must be relative to the Compose file"
            )));
        }
        let path = fs::canonicalize(compose_dir.join(&definition.file)).map_err(|source| {
            ComposeError::Read {
                path: compose_dir.join(&definition.file),
                source,
            }
        })?;
        if !path.starts_with(compose_dir) {
            return Err(ComposeError::Invalid(format!(
                "{kind} {name:?} file must not escape the Compose directory"
            )));
        }
        let data = fs::read(&path).map_err(|source| ComposeError::Read { path, source })?;
        if data.contains(&0) {
            return Err(ComposeError::Invalid(format!(
                "{kind} {name:?} contains a NUL byte"
            )));
        }
        configs.insert(name, data);
    }
    Ok(configs)
}

fn image_config_plan(
    service: &str,
    mounts: Vec<ComposeServiceConfig>,
    definitions: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<ImageConfigPlan>, ComposeError> {
    let mut targets = BTreeSet::new();
    let mut result = Vec::new();
    for mount in mounts {
        let (source, target) = match mount {
            ComposeServiceConfig::Name(source) => {
                let target = format!("/{source}");
                (source, target)
            }
            ComposeServiceConfig::Mount(mount) => {
                let target = mount.target.unwrap_or_else(|| format!("/{}", mount.source));
                (mount.source, target)
            }
        };
        let Some(data) = definitions.get(&source) else {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} references unknown config {source:?}"
            )));
        };
        let path = Path::new(&target);
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            || target.contains('\0')
        {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} config target {target:?} must be an absolute path without parent traversal"
            )));
        }
        if !targets.insert(target.clone()) {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} mounts more than one config at {target:?}"
            )));
        }
        result.push(ImageConfigPlan {
            target,
            data: data.clone(),
        });
    }
    Ok(result)
}

fn image_launch_plan(
    service: &str,
    command: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
    working_dir: Option<String>,
    user: Option<String>,
    read_only: bool,
    tmpfs: Vec<String>,
) -> Result<Option<ImageLaunchPlan>, ComposeError> {
    for (field, values) in [
        ("command", command.as_ref()),
        ("entrypoint", entrypoint.as_ref()),
    ] {
        let Some(values) = values else {
            continue;
        };
        for value in values {
            if value.contains('\0') {
                return Err(ComposeError::Invalid(format!(
                    "service {service:?} {field} contains a NUL byte"
                )));
            }
        }
    }
    if entrypoint
        .as_ref()
        .is_some_and(|values| !values.is_empty() && !values[0].starts_with('/'))
    {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} entrypoint program must be an absolute path"
        )));
    }
    if let Some(directory) = &working_dir {
        if directory.contains('\0') {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} working_dir contains a NUL byte"
            )));
        }
        if !directory.starts_with('/') {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} working_dir must be an absolute path"
            )));
        }
    }
    let user = user
        .map(|user| image_user_plan(service, &user))
        .transpose()?;
    for path in &tmpfs {
        if !path.starts_with('/') || path.contains('\0') || path.split('/').any(|part| part == "..")
        {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} tmpfs path {path:?} must be absolute without parent traversal"
            )));
        }
    }
    if command.is_none()
        && entrypoint.is_none()
        && working_dir.is_none()
        && user.is_none()
        && !read_only
        && tmpfs.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(ImageLaunchPlan {
        command,
        entrypoint,
        working_dir,
        user,
        read_only,
        tmpfs,
    }))
}

fn image_user_plan(service: &str, value: &str) -> Result<ImageUserPlan, ComposeError> {
    let Some((uid, gid)) = value.split_once(':') else {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} user must use numeric uid:gid"
        )));
    };
    if uid.is_empty() || gid.is_empty() || gid.contains(':') {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} user must use numeric uid:gid"
        )));
    }
    let uid = uid.parse::<u32>().map_err(|_| {
        ComposeError::Invalid(format!("service {service:?} user must use numeric uid:gid"))
    })?;
    let gid = gid.parse::<u32>().map_err(|_| {
        ComposeError::Invalid(format!("service {service:?} user must use numeric uid:gid"))
    })?;
    Ok(ImageUserPlan { uid, gid })
}

/// Map Compose's resource declarations to the machine configuration that is
/// already part of every locked Theseus run plan. Fractional CPU quotas would
/// need a host scheduler or timer-driven throttler, so certification supports
/// only an integral VM vCPU count. Memory is likewise rounded nowhere: users
/// must declare an integral MiB quantity.
fn apply_resource_limits(
    service: &str,
    run: &mut RunPlan,
    cpus: Option<ComposeQuantity>,
    mem_limit: Option<ComposeQuantity>,
    deploy: Option<ComposeResourceLimits>,
) -> Result<(), ComposeError> {
    let deploy_cpus = deploy.as_ref().and_then(|limits| limits.cpus.as_ref());
    let deploy_memory = deploy.as_ref().and_then(|limits| limits.memory.as_ref());
    let cpus = compose_matching_quantity(service, "cpus", cpus.as_ref(), deploy_cpus)?;
    let memory = compose_matching_quantity(service, "memory", mem_limit.as_ref(), deploy_memory)?;
    if let Some(value) = cpus {
        run.run.vcpu_count = parse_compose_vcpus(service, &value)?;
    }
    if let Some(value) = memory {
        run.run.mem_size_mib = parse_compose_memory_mib(service, &value)?;
    }
    Ok(())
}

fn compose_matching_quantity(
    service: &str,
    field: &str,
    service_value: Option<&ComposeQuantity>,
    deploy_value: Option<&ComposeQuantity>,
) -> Result<Option<String>, ComposeError> {
    let service_value = service_value.map(ComposeQuantity::literal);
    let deploy_value = deploy_value.map(ComposeQuantity::literal);
    if let (Some(left), Some(right)) = (&service_value, &deploy_value) {
        if left != right {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} {field} and deploy.resources.limits.{field} must agree"
            )));
        }
    }
    Ok(service_value.or(deploy_value))
}

fn parse_compose_vcpus(service: &str, value: &str) -> Result<u8, ComposeError> {
    let count = value.parse::<u8>().ok().filter(|count| *count > 0);
    count.ok_or_else(|| {
        ComposeError::Invalid(format!(
            "service {service:?} cpus must be a whole number from 1 to 255"
        ))
    })
}

fn parse_compose_memory_mib(service: &str, value: &str) -> Result<u32, ComposeError> {
    let value = value.trim();
    let number = value
        .strip_suffix("MiB")
        .or_else(|| value.strip_suffix("mib"))
        .or_else(|| value.strip_suffix('M'))
        .or_else(|| value.strip_suffix('m'))
        .unwrap_or(value);
    let mib = number.parse::<u32>().ok().filter(|mib| *mib > 0);
    mib.ok_or_else(|| {
        ComposeError::Invalid(format!(
            "service {service:?} memory must be a positive whole MiB quantity (for example 256M)"
        ))
    })
}

fn environment_plan(
    service: &str,
    compose_dir: &Path,
    env_files: Option<ComposeEnvFiles>,
    environment: Option<ComposeEnvironment>,
) -> Result<BTreeMap<String, String>, ComposeError> {
    let mut result = environment_files_plan(service, compose_dir, env_files)?;
    let entries = match environment {
        None => return Ok(result),
        Some(ComposeEnvironment::Map(entries)) => entries.into_iter().collect(),
        Some(ComposeEnvironment::List(entries)) => entries
            .into_iter()
            .map(|entry| {
                entry.split_once('=').map_or_else(
                    || {
                        Err(ComposeError::Invalid(format!(
                            "service {service:?} environment entry {entry:?} must use KEY=value; host-environment inheritance is not supported"
                        )))
                    },
                    |(key, value)| Ok((key.to_owned(), value.to_owned())),
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let mut explicit_names = BTreeSet::new();
    for (key, value) in entries {
        validate_environment_value(service, &key, &value)?;
        if !explicit_names.insert(key.clone()) {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} environment names {key:?} more than once"
            )));
        }
        // Explicit `environment` is the final Compose precedence layer and
        // intentionally replaces a same-named value from env_file.
        result.insert(key, value);
    }
    Ok(result)
}

fn environment_files_plan(
    service: &str,
    compose_dir: &Path,
    env_files: Option<ComposeEnvFiles>,
) -> Result<BTreeMap<String, String>, ComposeError> {
    let paths = match env_files {
        None => return Ok(BTreeMap::new()),
        Some(ComposeEnvFiles::Single(path)) => vec![path],
        Some(ComposeEnvFiles::List(paths)) => paths,
    };
    let mut result = BTreeMap::new();
    for file in paths {
        if Path::new(&file).is_absolute() {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} env_file {file:?} must be relative to the Compose file"
            )));
        }
        let path =
            fs::canonicalize(compose_dir.join(&file)).map_err(|source| ComposeError::Read {
                path: compose_dir.join(&file),
                source,
            })?;
        if !path.starts_with(compose_dir) {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} env_file {file:?} must not escape the Compose directory"
            )));
        }
        let input =
            fs::read_to_string(&path).map_err(|source| ComposeError::Read { path, source })?;
        for (line_number, line) in input.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ComposeError::Invalid(format!(
                    "service {service:?} env_file {file:?} line {} must use KEY=value; host-environment inheritance is not supported",
                    line_number + 1
                )));
            };
            validate_environment_value(service, key, value)?;
            result.insert(key.to_owned(), value.to_owned());
        }
    }
    Ok(result)
}

fn validate_environment_value(service: &str, key: &str, value: &str) -> Result<(), ComposeError> {
    if key.is_empty()
        || !key.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
    {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} environment key {key:?} must be a shell variable name"
        )));
    }
    if key == "THESEUS_CHANNEL" {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} environment cannot override THESEUS_CHANNEL"
        )));
    }
    if value.contains('\0') {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} environment value for {key:?} contains NUL"
        )));
    }
    if value.contains("${") {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} environment value for {key:?} must be literal; interpolation is not supported"
        )));
    }
    Ok(())
}

fn host_identity_plan(
    service: &str,
    hostname: Option<String>,
    extra_hosts: Option<ComposeExtraHosts>,
) -> Result<(Option<String>, BTreeMap<String, String>), ComposeError> {
    if let Some(hostname) = &hostname {
        validate_host_name(service, "hostname", hostname)?;
    }
    let entries = match extra_hosts {
        None => Vec::new(),
        Some(ComposeExtraHosts::Map(entries)) => entries.into_iter().collect(),
        Some(ComposeExtraHosts::List(entries)) => entries
            .into_iter()
            .map(|entry| {
                let Some((name, address)) = entry
                    .split_once('=')
                    .or_else(|| entry.split_once(':'))
                else {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} extra_hosts entry {entry:?} must use name=address or name:address"
                    )));
                };
                Ok((name.to_owned(), address.to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let mut hosts = BTreeMap::new();
    for (name, address) in entries {
        validate_host_name(service, "extra_hosts name", &name)?;
        let address = address.parse::<IpAddr>().map_err(|_| {
            ComposeError::Invalid(format!(
                "service {service:?} extra_hosts address for {name:?} must be an IP address"
            ))
        })?;
        if hosts.insert(name.clone(), address.to_string()).is_some() {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} extra_hosts names {name:?} more than once"
            )));
        }
    }
    Ok((hostname, hosts))
}

fn validate_host_name(service: &str, field: &str, value: &str) -> Result<(), ComposeError> {
    if value.is_empty()
        || value.len() > 253
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || !label.as_bytes()[0].is_ascii_alphanumeric()
                || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(ComposeError::Invalid(format!(
            "service {service:?} {field} {value:?} must be a DNS host name"
        )));
    }
    Ok(())
}

fn dependency_plan(
    source: &str,
    dependencies: Option<ComposeDependencies>,
) -> Result<Vec<DependencyPlan>, ComposeError> {
    let Some(dependencies) = dependencies else {
        return Ok(Vec::new());
    };
    let entries: Vec<(String, DependencyCondition)> = match dependencies {
        ComposeDependencies::Names(names) => names
            .into_iter()
            .map(|service| (service, DependencyCondition::ServiceStarted))
            .collect(),
        ComposeDependencies::Conditions(conditions) => conditions
            .into_iter()
            .map(|(service, dependency)| (service, dependency.condition))
            .collect(),
    };
    let mut plans = Vec::with_capacity(entries.len());
    let mut seen = BTreeSet::new();
    for (service, condition) in entries {
        validate_name("depends_on service", &service)?;
        if service == source {
            return Err(ComposeError::Invalid(format!(
                "service {source:?} cannot depend on itself"
            )));
        }
        if !seen.insert(service.clone()) {
            return Err(ComposeError::Invalid(format!(
                "service {source:?} lists dependency {service:?} more than once"
            )));
        }
        plans.push(DependencyPlan { service, condition });
    }
    plans.sort_by(|left, right| left.service.cmp(&right.service));
    Ok(plans)
}

fn validate_dependency_graph(
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<(), ComposeError> {
    for (name, service) in services {
        for dependency in &service.depends_on {
            let Some(target) = services.get(&dependency.service) else {
                return Err(ComposeError::Invalid(format!(
                    "service {name:?} depends on unknown service {:?}",
                    dependency.service
                )));
            };
            if dependency.condition == DependencyCondition::ServiceHealthy
                && target.run.container_service.is_none()
                && target.healthcheck.is_none()
            {
                return Err(ComposeError::Invalid(format!(
                    "service {name:?} requires healthy dependency {:?}, but it has no Compose healthcheck or container_service readiness contract",
                    dependency.service
                )));
            }
        }
    }
    fn visit(
        service: &str,
        services: &BTreeMap<String, ComposeServicePlan>,
        active: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
    ) -> Result<(), ComposeError> {
        if complete.contains(service) {
            return Ok(());
        }
        if !active.insert(service.to_owned()) {
            return Err(ComposeError::Invalid(format!(
                "Compose depends_on graph contains a cycle at service {service:?}"
            )));
        }
        for dependency in &services[service].depends_on {
            visit(&dependency.service, services, active, complete)?;
        }
        active.remove(service);
        complete.insert(service.to_owned());
        Ok(())
    }

    let mut active = BTreeSet::new();
    let mut complete = BTreeSet::new();
    for service in services.keys() {
        visit(service, services, &mut active, &mut complete)?;
    }
    Ok(())
}

fn default_campaign_runs() -> u16 {
    32
}

fn default_test_command_parallelism() -> u8 {
    2
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_http_status() -> u16 {
    200
}

fn default_grpc_status() -> GrpcServingStatus {
    GrpcServingStatus::Serving
}

fn default_shell_exit() -> i32 {
    0
}

fn default_campaign_faults_per_run() -> u8 {
    2
}

fn default_campaign_operations_per_run() -> u8 {
    3
}

const MAX_THREAD_SCHEDULE_SEARCH_PATTERNS: usize = 256;
const MAX_THREAD_SCHEDULE_SEARCH_PERIOD: u8 = 16;
const MAX_STRUCTURED_CHOICE_CASES: usize = 256;

fn structured_choice_assignments(
    bounds: &BTreeMap<String, u16>,
    operation: &str,
) -> Result<Vec<BTreeMap<String, u16>>, ComposeError> {
    let mut assignments = vec![BTreeMap::new()];
    for (name, bound) in bounds {
        validate_name("structured choice", name)?;
        if name.len() > 64 {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} structured choice {name:?} must be at most 64 bytes"
            )));
        }
        if *bound == 0 || *bound > 256 {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} structured choice {name:?} must have a bound between 1 and 256"
            )));
        }
        if assignments.len().saturating_mul(usize::from(*bound)) > MAX_STRUCTURED_CHOICE_CASES {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} generates more than {MAX_STRUCTURED_CHOICE_CASES} structured choice assignments"
            )));
        }
        assignments = assignments
            .into_iter()
            .flat_map(|assignment| {
                (0..*bound).map(move |selected| {
                    let mut candidate = assignment.clone();
                    candidate.insert(name.clone(), selected);
                    candidate
                })
            })
            .collect();
    }
    Ok(assignments)
}

fn structured_choices_environment(choices: &BTreeMap<String, u16>) -> String {
    choices
        .iter()
        .map(|(name, selected)| format!("{name}={selected}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn shell_input_name(
    schedule: &[u8],
    searched_schedule: bool,
    choices: &BTreeMap<String, u16>,
) -> String {
    let mut parts = Vec::new();
    if searched_schedule {
        parts.push(format!(
            "schedule-{}",
            schedule
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join("-")
        ));
    }
    parts.extend(
        choices
            .iter()
            .map(|(name, selected)| format!("{name}-{selected}")),
    );
    if parts.is_empty() {
        "default".to_owned()
    } else {
        parts.join("+")
    }
}

fn thread_schedule_search_patterns(
    search: &ComposeThreadScheduleSearch,
    operation: &str,
) -> Result<Vec<Vec<u8>>, ComposeError> {
    let unique = search.threads.iter().copied().collect::<BTreeSet<_>>();
    if search.threads.len() < 2
        || unique.len() != search.threads.len()
        || !unique.contains(&0)
        || search.threads.iter().any(|thread| *thread >= 32)
        || search.period == 0
        || search.period > MAX_THREAD_SCHEDULE_SEARCH_PERIOD
        || search.max_switches > search.period
    {
        return Err(ComposeError::Invalid(format!(
            "campaign operation {operation:?} has an invalid thread schedule search"
        )));
    }

    fn extend(
        threads: &[u8],
        period: usize,
        maximum_switches: usize,
        prefix: &mut Vec<u8>,
        switches: usize,
        output: &mut Vec<Vec<u8>>,
    ) -> bool {
        if prefix.len() == period {
            let wrap_switch = usize::from(
                prefix
                    .first()
                    .zip(prefix.last())
                    .is_some_and(|(first, last)| first != last),
            );
            if switches + wrap_switch <= maximum_switches {
                output.push(prefix.clone());
            }
            return output.len() <= MAX_THREAD_SCHEDULE_SEARCH_PATTERNS;
        }
        for thread in threads {
            let next_switches =
                switches + usize::from(prefix.last().is_some_and(|previous| previous != thread));
            if next_switches > maximum_switches {
                continue;
            }
            prefix.push(*thread);
            if !extend(
                threads,
                period,
                maximum_switches,
                prefix,
                next_switches,
                output,
            ) {
                return false;
            }
            prefix.pop();
        }
        true
    }

    let mut patterns = Vec::new();
    if !extend(
        &search.threads,
        usize::from(search.period),
        usize::from(search.max_switches),
        &mut Vec::with_capacity(usize::from(search.period)),
        0,
        &mut patterns,
    ) {
        return Err(ComposeError::Invalid(format!(
            "campaign operation {operation:?} thread schedule search generates more than {MAX_THREAD_SCHEDULE_SEARCH_PATTERNS} patterns"
        )));
    }
    Ok(patterns)
}

const TEST_COMMAND_ROOT: &str = "opt/antithesis/test/v1";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerSaveManifest {
    layers: Vec<String>,
}

#[derive(Clone, Copy)]
struct ImagePathKind {
    runnable: bool,
}

fn archive_path(path: &Path) -> Result<String, ComposeError> {
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(component) => normalized.push(
                component
                    .to_str()
                    .ok_or_else(|| {
                        ComposeError::Invalid("image contains a non-UTF-8 path".to_owned())
                    })?
                    .to_owned(),
            ),
            std::path::Component::CurDir | std::path::Component::RootDir => {}
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(ComposeError::Invalid(format!(
                    "image contains unsafe path {:?}",
                    path
                )))
            }
        }
    }
    Ok(normalized.join("/"))
}

fn apply_test_command_layer(
    layer: &[u8],
    paths: &mut BTreeMap<String, ImagePathKind>,
) -> Result<(), ComposeError> {
    let reader: Box<dyn Read> = if layer.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(layer))
    } else {
        Box::new(layer)
    };
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|error| {
        ComposeError::Invalid(format!(
            "cannot read image layer for test commands: {error}"
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            ComposeError::Invalid(format!("cannot read image layer entry: {error}"))
        })?;
        let path = archive_path(&entry.path().map_err(|error| {
            ComposeError::Invalid(format!("cannot read image layer path: {error}"))
        })?)?;
        let (parent, name) = path
            .rsplit_once('/')
            .map_or(("", path.as_str()), |(parent, name)| (parent, name));
        if name == ".wh..wh..opq" {
            let prefix = format!("{parent}/");
            paths.retain(|candidate, _| !candidate.starts_with(&prefix));
            continue;
        }
        if let Some(removed) = name.strip_prefix(".wh.") {
            let removed = if parent.is_empty() {
                removed.to_owned()
            } else {
                format!("{parent}/{removed}")
            };
            let prefix = format!("{removed}/");
            paths.retain(|candidate, _| candidate != &removed && !candidate.starts_with(&prefix));
            continue;
        }
        let kind = entry.header().entry_type();
        if kind.is_file() || kind.is_hard_link() || kind.is_symlink() {
            let mode = entry.header().mode().map_err(|error| {
                ComposeError::Invalid(format!("cannot read image mode for {path:?}: {error}"))
            })?;
            paths.insert(
                path,
                ImagePathKind {
                    runnable: kind.is_symlink() || mode & 0o111 != 0,
                },
            );
        } else if kind.is_dir() {
            paths.remove(&path);
        }
    }
    Ok(())
}

fn image_paths(image: &Path) -> Result<BTreeMap<String, ImagePathKind>, ComposeError> {
    let bytes = fs::read(image).map_err(|source| ComposeError::Read {
        path: image.to_path_buf(),
        source,
    })?;
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut manifest = None;
    let mut layers = BTreeMap::new();
    for entry in archive.entries().map_err(|error| {
        ComposeError::Invalid(format!(
            "cannot read image archive {}: {error}",
            image.display()
        ))
    })? {
        let mut entry = entry.map_err(|error| {
            ComposeError::Invalid(format!("cannot read image archive entry: {error}"))
        })?;
        let path = archive_path(&entry.path().map_err(|error| {
            ComposeError::Invalid(format!("cannot read image archive path: {error}"))
        })?)?;
        if path == "manifest.json" || path.ends_with("/manifest.json") {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(|error| {
                ComposeError::Invalid(format!("cannot read image manifest: {error}"))
            })?;
            manifest = Some(
                serde_json::from_slice::<Vec<DockerSaveManifest>>(&data).map_err(|error| {
                    ComposeError::Invalid(format!("cannot parse image manifest: {error}"))
                })?,
            );
        } else if entry.header().entry_type().is_file() && !path.ends_with(".json") {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(|error| {
                ComposeError::Invalid(format!("cannot read image layer {path:?}: {error}"))
            })?;
            layers.insert(path, data);
        }
    }
    let manifest = manifest
        .and_then(|entries| entries.into_iter().next())
        .ok_or_else(|| ComposeError::Invalid("image has no Docker save manifest".to_owned()))?;
    let mut paths = BTreeMap::new();
    for name in manifest.layers {
        let layer = layers.get(&name).ok_or_else(|| {
            ComposeError::Invalid(format!("image manifest references missing layer {name:?}"))
        })?;
        apply_test_command_layer(layer, &mut paths)?;
    }
    Ok(paths)
}

fn test_command_role(filename: &str) -> Option<ComposeTestCommand> {
    [
        ("parallel_driver_", ComposeTestCommand::ParallelDriver),
        ("singleton_driver_", ComposeTestCommand::SingletonDriver),
        ("serial_driver_", ComposeTestCommand::SerialDriver),
        ("eventually_", ComposeTestCommand::Eventually),
        ("finally_", ComposeTestCommand::Finally),
        ("anytime_", ComposeTestCommand::Anytime),
        ("first_", ComposeTestCommand::First),
    ]
    .into_iter()
    .find_map(|(prefix, role)| {
        (filename.starts_with(prefix) && filename.len() > prefix.len()).then_some(role)
    })
}

fn discovered_operation_name(
    template: Option<&str>,
    service: &str,
    filename: &str,
    suffix: &str,
) -> String {
    let mut name = match template {
        Some(template) => format!("{template}_{service}_{filename}{suffix}"),
        None => format!("{service}_{filename}{suffix}"),
    };
    name.retain(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    name
}

fn discovered_operation(
    name: String,
    template: &str,
    service: &str,
    role: ComposeTestCommand,
    path: &str,
    phase: ComposeShellPhase,
    process: Option<String>,
) -> ComposeOperation {
    ComposeOperation {
        name,
        test_template: Some(template.to_owned()),
        test_command_path: Some(path.to_owned()),
        command: Some(role),
        service: Some(service.to_owned()),
        shell: Some(ComposeShellOperation {
            phase,
            process,
            command: if matches!(phase, ComposeShellPhase::Completion) {
                Vec::new()
            } else {
                vec![path.to_owned()]
            },
            ..ComposeShellOperation::default()
        }),
        max_uses: Some(1),
        ..ComposeOperation::default()
    }
}

fn discover_test_template(
    template: &str,
    max_parallel_commands: u8,
    services: &BTreeMap<String, ComposeServicePlan>,
    qualify_names: bool,
) -> Result<Vec<ComposeOperation>, ComposeError> {
    validate_name("test template", template)?;
    if !(1..=8).contains(&max_parallel_commands) {
        return Err(ComposeError::Invalid(
            "campaign max_parallel_commands must be between 1 and 8".to_owned(),
        ));
    }
    let directory = format!("{TEST_COMMAND_ROOT}/{template}/");
    let mut operations = Vec::new();
    for (service, plan) in services {
        let Some(image) = &plan.run.guest.image else {
            continue;
        };
        for (path, kind) in image_paths(Path::new(&image.path))? {
            let Some(filename) = path.strip_prefix(&directory) else {
                continue;
            };
            if filename.contains('/') || filename.starts_with("helper_") {
                continue;
            }
            let Some(role) = test_command_role(filename) else {
                continue;
            };
            if !kind.runnable {
                return Err(ComposeError::Invalid(format!(
                    "test command /{path} in service {service:?} is not executable"
                )));
            }
            let command_path = format!("/{path}");
            if matches!(
                role,
                ComposeTestCommand::ParallelDriver | ComposeTestCommand::Anytime
            ) {
                for slot in 1..=max_parallel_commands {
                    let suffix = format!("_{slot}");
                    let qualifier = qualify_names.then_some(template);
                    let process = discovered_operation_name(qualifier, service, filename, &suffix);
                    operations.push(discovered_operation(
                        discovered_operation_name(
                            qualifier,
                            service,
                            filename,
                            &format!("_start_{slot}"),
                        ),
                        template,
                        service,
                        role,
                        &command_path,
                        ComposeShellPhase::Launch,
                        Some(process.clone()),
                    ));
                    operations.push(discovered_operation(
                        discovered_operation_name(
                            qualifier,
                            service,
                            filename,
                            &format!("_finish_{slot}"),
                        ),
                        template,
                        service,
                        role,
                        &command_path,
                        ComposeShellPhase::Completion,
                        Some(process),
                    ));
                }
            } else {
                operations.push(discovered_operation(
                    discovered_operation_name(
                        qualify_names.then_some(template),
                        service,
                        filename,
                        "",
                    ),
                    template,
                    service,
                    role,
                    &command_path,
                    ComposeShellPhase::Run,
                    None,
                ));
            }
            if operations.len() > 256 {
                return Err(ComposeError::Invalid(
                    "selected test template expands to more than 256 operations".to_owned(),
                ));
            }
        }
    }
    if operations.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "test template {template:?} has no recognized executable commands under /{TEST_COMMAND_ROOT}"
        )));
    }
    Ok(operations)
}

fn discovered_test_template_names(
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<Vec<String>, ComposeError> {
    let prefix = format!("{TEST_COMMAND_ROOT}/");
    let mut templates = BTreeSet::new();
    for plan in services.values() {
        let Some(image) = &plan.run.guest.image else {
            continue;
        };
        for path in image_paths(Path::new(&image.path))?.keys() {
            let Some(relative) = path.strip_prefix(&prefix) else {
                continue;
            };
            let Some((template, filename)) = relative.split_once('/') else {
                continue;
            };
            if template.is_empty()
                || filename.contains('/')
                || filename.starts_with("helper_")
                || test_command_role(filename).is_none()
            {
                continue;
            }
            validate_name("test template", template)?;
            templates.insert(template.to_owned());
        }
    }
    if templates.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign has no explicit operations and no test templates under /{TEST_COMMAND_ROOT}"
        )));
    }
    if templates.len() > 32 {
        return Err(ComposeError::Invalid(
            "campaign discovers more than 32 test templates".to_owned(),
        ));
    }
    Ok(templates.into_iter().collect())
}

fn campaign_plan(
    campaign: Option<ComposeTheseus>,
    services: &mut BTreeMap<String, ComposeServicePlan>,
) -> Result<Option<CampaignPlan>, ComposeError> {
    let Some(campaign) = campaign.and_then(|theseus| theseus.campaign) else {
        return Ok(None);
    };
    if !services.contains_key(&campaign.driver) {
        return Err(ComposeError::Invalid(format!(
            "campaign driver {:?} is not a service",
            campaign.driver
        )));
    }
    let test_template = campaign.test_template.clone();
    let mut requested_templates = campaign.test_templates.clone();
    if test_template.is_some() && !requested_templates.is_empty() {
        return Err(ComposeError::Invalid(
            "campaign must use only one of test_template or test_templates".to_owned(),
        ));
    }
    if (!requested_templates.is_empty() || test_template.is_some())
        && !campaign.operations.is_empty()
    {
        return Err(ComposeError::Invalid(
            "campaign test templates replace operations; do not declare both".to_owned(),
        ));
    }
    if requested_templates.is_empty() && test_template.is_none() && campaign.operations.is_empty() {
        requested_templates = discovered_test_template_names(services)?;
    }
    let selected_templates = test_template
        .iter()
        .cloned()
        .chain(requested_templates.iter().cloned())
        .collect::<Vec<_>>();
    let mut distinct_templates = BTreeSet::new();
    for template in &selected_templates {
        validate_name("test template", template)?;
        if !distinct_templates.insert(template.as_str()) {
            return Err(ComposeError::Invalid(format!(
                "test template {template:?} is selected more than once"
            )));
        }
    }
    let campaign_operations = if selected_templates.is_empty() {
        campaign.operations
    } else {
        let qualify_names = selected_templates.len() > 1;
        let mut operations = Vec::new();
        for template in &selected_templates {
            operations.extend(discover_test_template(
                template,
                campaign.max_parallel_commands,
                services,
                qualify_names,
            )?);
            if operations.len() > 256 {
                return Err(ComposeError::Invalid(
                    "selected test templates expand to more than 256 operations".to_owned(),
                ));
            }
        }
        operations
    };
    if campaign_operations.is_empty() {
        return Err(ComposeError::Invalid(
            "campaign needs operations or a discovered test template".to_owned(),
        ));
    }
    if campaign.max_runs == 0 || campaign.max_runs > 256 {
        return Err(ComposeError::Invalid(
            "campaign max_runs must be between 1 and 256".to_owned(),
        ));
    }
    if campaign.max_faults_per_run == 0 || campaign.max_faults_per_run > 4 {
        return Err(ComposeError::Invalid(
            "campaign max_faults_per_run must be between 1 and 4".to_owned(),
        ));
    }
    if campaign.max_operations_per_run == 0 || campaign.max_operations_per_run > 12 {
        return Err(ComposeError::Invalid(
            "campaign max_operations_per_run must be between 1 and 12".to_owned(),
        ));
    }
    let initial_state = campaign.state;
    let evidence_definitions = campaign.evidence;
    let mut resolved_evidence =
        normalize_serial_evidence_definitions(&evidence_definitions, services)?;
    let mut names = BTreeSet::new();
    let mut operations = Vec::with_capacity(campaign_operations.len());
    for operation in campaign_operations {
        validate_name("campaign operation", &operation.name)?;
        if !names.insert(operation.name.clone()) {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} is declared more than once",
                operation.name
            )));
        }
        let service = operation.service.unwrap_or_else(|| campaign.driver.clone());
        if !services.contains_key(&service) {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} targets unknown service {service:?}",
                operation.name
            )));
        }
        let http = operation.http;
        let grpc_health = operation.grpc_health;
        let shell = operation.shell;
        let shell_command = shell
            .as_ref()
            .map(|shell| shell.command.clone())
            .filter(|command| !command.is_empty());
        let input_forms = usize::from(operation.input.is_some())
            + usize::from(operation.input_template.is_some())
            + usize::from(!operation.inputs.is_empty())
            + usize::from(operation.input_grammar.is_some())
            + usize::from(http.is_some())
            + usize::from(grpc_health.is_some())
            + usize::from(shell.is_some());
        if input_forms > 1 {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} must use exactly one of input, input_template, inputs, or input_grammar (or http, grpc_health, or shell)",
                operation.name
            )));
        }
        let mut shell_phase = None;
        let mut shell_process = None;
        let mut thread_schedule = Vec::new();
        let mut thread_schedule_search = None;
        let mut thread_schedule_exploration = None;
        let mut choice_bounds = BTreeMap::new();
        let service_inputs = if let Some(http) = http {
            if !http.url.starts_with("http://")
                || !(100..=599).contains(&http.expect_status)
                || http.body_contains.as_deref() == Some("")
            {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} has an invalid HTTP contract",
                    operation.name
                )));
            }
            let Some(container) = services
                .get_mut(&service)
                .and_then(|service| service.run.container_service.as_mut())
            else {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} HTTP target {:?} needs container_service",
                    operation.name, service
                )));
            };
            container.campaign = true;
            let command = serde_json::json!({
                "name": operation.name.clone(),
                "method": http.method,
                "url": http.url,
                "body": http.body,
                "expect_status": http.expect_status,
                "body_contains": http.body_contains,
            });
            let command = serde_json::to_string(&command).expect("HTTP command is serializable");
            Some(vec![OperationInputPlan {
                name: "default".to_owned(),
                input_hex: hex(format!("THES:HTTP:operation:{command}\n").as_bytes()),
                choices: BTreeMap::new(),
                thread_schedule: Vec::new(),
                input_template: None,
                input_captures: BTreeMap::new(),
                requires: Vec::new(),
                excludes: Vec::new(),
                max_uses: None,
                requires_state: BTreeMap::new(),
                sets_state: BTreeMap::new(),
            }])
        } else if let Some(grpc_health) = grpc_health {
            if !grpc_health.url.starts_with("http://") {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} has an invalid gRPC health contract",
                    operation.name
                )));
            }
            let Some(container) = services
                .get_mut(&service)
                .and_then(|service| service.run.container_service.as_mut())
            else {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} gRPC health target {:?} needs container_service",
                    operation.name, service
                )));
            };
            container.campaign = true;
            let command = serde_json::json!({
                "name": operation.name.clone(),
                "url": grpc_health.url,
                "service": grpc_health.service,
                "expect_status": grpc_health.expect_status,
            });
            let command = serde_json::to_string(&command).expect("gRPC command is serializable");
            Some(vec![OperationInputPlan {
                name: "default".to_owned(),
                input_hex: hex(format!("THES:GRPC:operation:{command}\n").as_bytes()),
                choices: BTreeMap::new(),
                thread_schedule: Vec::new(),
                input_template: None,
                input_captures: BTreeMap::new(),
                requires: Vec::new(),
                excludes: Vec::new(),
                max_uses: None,
                requires_state: BTreeMap::new(),
                sets_state: BTreeMap::new(),
            }])
        } else if let Some(shell) = shell {
            let choice_assignments =
                structured_choice_assignments(&shell.choices, &operation.name)?;
            if choice_assignments
                .len()
                .saturating_mul(match &shell.thread_schedule {
                    ComposeThreadSchedule::Exact(_) => 1,
                    ComposeThreadSchedule::Search(search) => {
                        thread_schedule_search_patterns(search, &operation.name)?.len()
                    }
                    ComposeThreadSchedule::Exploration(_) => 1,
                })
                > MAX_STRUCTURED_CHOICE_CASES
            {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} generates more than {MAX_STRUCTURED_CHOICE_CASES} combined schedule and structured choice cases",
                    operation.name
                )));
            }
            choice_bounds = shell.choices.clone();
            let named_process = matches!(
                shell.phase,
                ComposeShellPhase::Launch | ComposeShellPhase::Completion
            );
            let command_required = !matches!(shell.phase, ComposeShellPhase::Completion);
            let command_valid = (!command_required && shell.command.is_empty())
                || (command_required
                    && !shell.command.is_empty()
                    && shell.command[0].starts_with('/')
                    && shell
                        .command
                        .iter()
                        .all(|argument| !argument.is_empty() && !argument.contains('\0')));
            let process_valid = match (named_process, shell.process.as_deref()) {
                (true, Some(process)) => {
                    !process.is_empty()
                        && process.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                        })
                }
                (false, None) => true,
                _ => false,
            };
            let (schedule_patterns, search) = match &shell.thread_schedule {
                ComposeThreadSchedule::Exact(schedule) => {
                    if schedule.len() > 128 || schedule.iter().any(|thread| *thread >= 32) {
                        return Err(ComposeError::Invalid(format!(
                            "campaign operation {:?} has an invalid thread schedule",
                            operation.name
                        )));
                    }
                    (vec![schedule.clone()], None)
                }
                ComposeThreadSchedule::Search(search) => {
                    let patterns = thread_schedule_search_patterns(search, &operation.name)?;
                    let plan = ThreadScheduleSearchPlan {
                        threads: search.threads.clone(),
                        period: search.period,
                        max_switches: search.max_switches,
                        generated_schedules: patterns.len(),
                    };
                    (patterns, Some(plan))
                }
                ComposeThreadSchedule::Exploration(exploration) => {
                    let exploration = &exploration.runnable_prefixes;
                    if exploration.max_choices == 0
                        || exploration.max_choices > 128
                        || exploration.max_variants < 2
                        || usize::from(exploration.max_variants)
                            > MAX_THREAD_SCHEDULE_SEARCH_PATTERNS
                    {
                        return Err(ComposeError::Invalid(format!(
                            "campaign operation {:?} has an invalid runnable-prefix exploration",
                            operation.name
                        )));
                    }
                    thread_schedule_exploration = Some(ThreadScheduleExplorationPlan {
                        strategy: "runnable_prefixes",
                        max_choices: exploration.max_choices,
                        max_variants: exploration.max_variants,
                    });
                    (vec![Vec::new()], None)
                }
            };
            let schedules_enabled = schedule_patterns
                .iter()
                .any(|schedule| !schedule.is_empty());
            let exploring_schedule = thread_schedule_exploration.is_some();
            let schedule_valid = !shell.environment.contains_key("THESEUS_THREAD_SCHEDULE")
                && !shell
                    .environment
                    .contains_key("THESEUS_THREAD_SCHEDULE_MODE")
                && ((!schedules_enabled && !exploring_schedule)
                    || !matches!(
                        shell.phase,
                        ComposeShellPhase::Launch | ComposeShellPhase::Completion
                    ));
            if !command_valid
                || !process_valid
                || !schedule_valid
                || (matches!(shell.phase, ComposeShellPhase::Launch)
                    && (shell.expect_exit != 0
                        || shell.output_contains.is_some()
                        || shell.output_json))
                || (matches!(shell.phase, ComposeShellPhase::Completion)
                    && !shell.environment.is_empty())
                || shell.output_contains.as_deref() == Some("")
                || shell.environment.iter().any(|(key, value)| {
                    key.is_empty()
                        || key.contains('=')
                        || key.contains('\0')
                        || value.contains('\0')
                        || key == "THESEUS_CHANNEL"
                        || key == "THESEUS_CHOICES"
                })
            {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} has an invalid shell contract",
                    operation.name
                )));
            }
            let Some(container) = services
                .get_mut(&service)
                .and_then(|service| service.run.container_service.as_mut())
            else {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} shell target {:?} needs container_service",
                    operation.name, service
                )));
            };
            container.campaign = true;
            shell_phase = Some(shell.phase);
            shell_process = shell.process.clone();
            if search.is_none() {
                thread_schedule = schedule_patterns[0].clone();
            }
            thread_schedule_search = search;
            let mut inputs = Vec::new();
            for schedule in schedule_patterns {
                for choices in &choice_assignments {
                    let mut environment = shell.environment.clone();
                    if exploring_schedule {
                        environment.insert(
                            "THESEUS_THREAD_SCHEDULE_MODE".to_owned(),
                            "runnable_prefix".to_owned(),
                        );
                        environment.insert("THESEUS_THREAD_SCHEDULE".to_owned(), String::new());
                    } else if !schedule.is_empty() {
                        environment.insert(
                            "THESEUS_THREAD_SCHEDULE".to_owned(),
                            schedule
                                .iter()
                                .map(u8::to_string)
                                .collect::<Vec<_>>()
                                .join(","),
                        );
                    }
                    if !choices.is_empty() {
                        environment.insert(
                            "THESEUS_CHOICES".to_owned(),
                            structured_choices_environment(choices),
                        );
                    }
                    let command = serde_json::json!({
                        "name": operation.name.clone(),
                        "phase": shell.phase,
                        "process": shell.process,
                        "command": shell.command,
                        "expect_exit": shell.expect_exit,
                        "output_contains": shell.output_contains,
                        "output_json": shell.output_json,
                        "environment": environment,
                    });
                    let command =
                        serde_json::to_string(&command).expect("shell command is serializable");
                    inputs.push(OperationInputPlan {
                        name: shell_input_name(
                            &schedule,
                            thread_schedule_search.is_some(),
                            choices,
                        ),
                        input_hex: hex(format!("THES:SHELL:operation:{command}\n").as_bytes()),
                        choices: choices.clone(),
                        thread_schedule: schedule.clone(),
                        input_template: None,
                        input_captures: BTreeMap::new(),
                        requires: Vec::new(),
                        excludes: Vec::new(),
                        max_uses: None,
                        requires_state: BTreeMap::new(),
                        sets_state: BTreeMap::new(),
                    });
                }
            }
            Some(inputs)
        } else {
            None
        };
        let grammar = operation.input_grammar;
        let input_grammar = grammar
            .as_ref()
            .map(|grammar| normalize_operation_input_grammar(grammar, &operation.name, services))
            .transpose()?;
        let has_input_captures = !operation.input_captures.is_empty();
        let captures = operation.input_captures;
        let input_template = operation
            .input_template
            .map(|template| {
                normalize_operation_input_template(template, captures, &operation.name, services)
            })
            .transpose()?;
        if input_template.is_none() && has_input_captures {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} input_captures require input_template",
                operation.name
            )));
        }
        let inputs = match (service_inputs, operation.input) {
            (Some(inputs), None) => inputs,
            (Some(_), Some(_)) => unreachable!("service operation input forms were validated"),
            (None, Some(input)) if input.is_empty() => {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} has empty input",
                    operation.name
                )));
            }
            (None, Some(input)) => vec![OperationInputPlan {
                name: "default".to_owned(),
                input_hex: hex(input.as_bytes()),
                choices: BTreeMap::new(),
                thread_schedule: Vec::new(),
                requires: Vec::new(),
                excludes: Vec::new(),
                max_uses: None,
                requires_state: BTreeMap::new(),
                sets_state: BTreeMap::new(),
                input_template: None,
                input_captures: BTreeMap::new(),
            }],
            (None, None)
                if operation.inputs.is_empty()
                    && input_grammar.is_none()
                    && input_template.is_none() =>
            {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} needs input, input_template, inputs, or input_grammar",
                    operation.name
                )));
            }
            (None, None) if input_template.is_some() => {
                let (template, captures) = input_template
                    .as_ref()
                    .expect("input template was normalized");
                vec![OperationInputPlan {
                    name: "default".to_owned(),
                    input_hex: String::new(),
                    choices: BTreeMap::new(),
                    thread_schedule: Vec::new(),
                    input_template: Some(template.clone()),
                    input_captures: captures.clone(),
                    requires: Vec::new(),
                    excludes: Vec::new(),
                    max_uses: None,
                    requires_state: BTreeMap::new(),
                    sets_state: BTreeMap::new(),
                }]
            }
            (None, None) if input_grammar.is_some() => input_grammar
                .as_ref()
                .expect("input grammar was normalized")
                .inputs
                .clone(),
            (None, None) => {
                let mut input_names = BTreeSet::new();
                operation
                    .inputs
                    .into_iter()
                    .map(|input| {
                        validate_name("campaign operation input", &input.name)?;
                        if !input_names.insert(input.name.clone()) {
                            return Err(ComposeError::Invalid(format!(
                                "campaign operation {:?} declares input {:?} more than once",
                                operation.name, input.name
                            )));
                        }
                        if input.input.is_empty() {
                            return Err(ComposeError::Invalid(format!(
                                "campaign operation {:?} input {:?} is empty",
                                operation.name, input.name
                            )));
                        }
                        Ok(OperationInputPlan {
                            name: input.name,
                            input_hex: hex(input.input.as_bytes()),
                            choices: BTreeMap::new(),
                            thread_schedule: Vec::new(),
                            input_template: None,
                            input_captures: BTreeMap::new(),
                            requires: normalize_operation_input_references(
                                input.requires,
                                "requires",
                                &operation.name,
                            )?,
                            excludes: normalize_operation_input_references(
                                input.excludes,
                                "excludes",
                                &operation.name,
                            )?,
                            max_uses: input.max_uses,
                            requires_state: input.requires_state,
                            sets_state: input.sets_state,
                        })
                    })
                    .collect::<Result<Vec<_>, ComposeError>>()?
            }
        };
        let context = format!("operation {:?}", operation.name);
        let requires_serial = operation
            .requires_serial
            .map(|guard| normalize_operation_serial_guard(guard, &context, services))
            .transpose()?;
        let excludes_serial = operation
            .excludes_serial
            .map(|guard| normalize_operation_serial_guard(guard, &context, services))
            .transpose()?;
        let requires_serial_all = normalize_operation_serial_guards(
            operation.requires_serial_all,
            "requires_serial_all",
            &context,
            services,
        )?;
        let excludes_serial_any = normalize_operation_serial_guards(
            operation.excludes_serial_any,
            "excludes_serial_any",
            &context,
            services,
        )?;
        let requires_serial_joins =
            normalize_serial_joins(operation.requires_serial_joins, &context, services)?;
        let excludes_serial_joins =
            normalize_serial_joins(operation.excludes_serial_joins, &context, services)?;
        let requires_serial_evidence = operation
            .requires_serial_evidence
            .map(|evidence| {
                normalize_serial_evidence_root(
                    evidence,
                    &context,
                    services,
                    &evidence_definitions,
                    &mut resolved_evidence,
                )
            })
            .transpose()?;
        let excludes_serial_evidence = operation
            .excludes_serial_evidence
            .map(|evidence| {
                normalize_serial_evidence_root(
                    evidence,
                    &context,
                    services,
                    &evidence_definitions,
                    &mut resolved_evidence,
                )
            })
            .transpose()?;
        operations.push(OperationPlan {
            name: operation.name,
            service,
            test_template: operation.test_template,
            command: operation.command,
            test_command_path: operation.test_command_path,
            shell_phase,
            shell_process,
            shell_command,
            thread_schedule,
            thread_schedule_search,
            thread_schedule_exploration,
            choice_bounds,
            inputs,
            input_grammar: input_grammar.map(|grammar| grammar.source),
            stage: operation.stage,
            requires: operation.requires,
            excludes: operation.excludes,
            requires_markers: operation.requires_markers,
            excludes_markers: operation.excludes_markers,
            requires_serial,
            excludes_serial,
            requires_serial_all,
            excludes_serial_any,
            requires_serial_joins,
            excludes_serial_joins,
            requires_serial_evidence,
            excludes_serial_evidence,
            max_uses: operation.max_uses,
            requires_state: operation.requires_state,
            sets_state: operation.sets_state,
        });
    }
    validate_campaign_operation_rules(&operations, &campaign.stages, &initial_state)?;
    validate_test_command_model(&operations, &campaign.stages)?;
    let mut campaign_faults = campaign.faults;
    let declared_fault_count = campaign_faults.len();
    if campaign.fault_profile == Some(ComposeFaultProfile::Standard) {
        campaign_faults.extend(standard_fault_profile(&operations, services));
    }
    let mut faults = Vec::with_capacity(campaign_faults.len());
    for (fault_index, candidate) in campaign_faults.into_iter().enumerate() {
        let generated_by_profile = fault_index >= declared_fault_count;
        let has_network_conditions = candidate.drop_ppm.is_some()
            || candidate.duplicate_ppm.is_some()
            || candidate.corrupt_ppm.is_some()
            || candidate.jitter_rounds.is_some()
            || candidate.tx_bytes_per_round.is_some()
            || candidate.mtu_bytes.is_some()
            || candidate.tx_queue_frames.is_some()
            || candidate.rx_queue_frames.is_some();
        if candidate.every_n_rounds.is_some()
            && !matches!(
                candidate.kind,
                CampaignFaultKind::CpuThrottle | CampaignFaultKind::CpuRelease
            )
        {
            return Err(ComposeError::Invalid(
                "every_n_rounds belongs to cpu_throttle/cpu_release actions".to_owned(),
            ));
        }
        if candidate.rate.is_some()
            && !matches!(
                candidate.kind,
                CampaignFaultKind::ClockRate | CampaignFaultKind::ClockRateRelease
            )
        {
            return Err(ComposeError::Invalid(
                "rate belongs to clock_rate/clock_rate_release actions".to_owned(),
            ));
        }
        let until = normalize_campaign_fault_window(&candidate, &operations)?;
        match candidate.kind {
            CampaignFaultKind::Pause
            | CampaignFaultKind::Restart
            | CampaignFaultKind::ClockJump => {
                if candidate.required {
                    return Err(ComposeError::Invalid(
                        "campaign lifecycle faults cannot be required; declare an always-applied fault under the service"
                            .to_owned(),
                    ));
                }
                let service_name = candidate.service.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign lifecycle fault requires service".to_owned())
                })?;
                let at_round = candidate.at_round.ok_or_else(|| {
                    ComposeError::Invalid("campaign lifecycle fault requires at_round".to_owned())
                })?;
                if candidate.network.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.after.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign lifecycle faults accept only service, at_round, duration_rounds, and nanoseconds"
                            .to_owned(),
                    ));
                }
                let service = services.get(service_name).ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "campaign fault references unknown service {service_name:?}",
                    ))
                })?;
                if service
                    .faults
                    .iter()
                    .any(|fault| fault.at_round == at_round)
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign fault for service {service_name:?} duplicates its fixed fault at round {at_round}",
                    )));
                }
                let kind = match candidate.kind {
                    CampaignFaultKind::Pause => FaultKind::Pause,
                    CampaignFaultKind::Restart => FaultKind::Restart,
                    CampaignFaultKind::ClockJump => FaultKind::ClockJump,
                    _ => unreachable!(),
                };
                let mut validated = validate_faults(
                    service_name,
                    vec![ComposeFault {
                        at_round,
                        kind,
                        duration_rounds: candidate.duration_rounds,
                        nanoseconds: candidate.nanoseconds,
                    }],
                    service.run.run.virtual_time.is_some(),
                )?;
                let fault = validated.pop().expect("one validated campaign fault");
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: None,
                    after: None,
                    after_input: None,
                    command: None,
                    until,
                    at_round: Some(fault.at_round),
                    duration_rounds: fault.duration_rounds,
                    nanoseconds: fault.nanoseconds,
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
                    rate: None,
                });
            }
            CampaignFaultKind::ServiceStop
            | CampaignFaultKind::ServiceStart
            | CampaignFaultKind::ServiceKill
            | CampaignFaultKind::ServiceRestart => {
                let service_name = candidate.service.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign service lifecycle action requires service".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign service lifecycle action requires after".to_owned(),
                    )
                })?;
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                let service = services.get_mut(service_name).ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "campaign lifecycle action references unknown service {service_name:?}"
                    ))
                })?;
                let Some(contract) = service.run.container_service.as_mut() else {
                    return Err(ComposeError::Invalid(format!(
                        "campaign lifecycle action requires image-backed service {service_name:?} with a container_service contract"
                    )));
                };
                contract.campaign = true;
                if candidate.network.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign service lifecycle actions accept only service and after"
                            .to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: None,
                    after,
                    after_input,
                    command: None,
                    until,
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
                    rate: None,
                });
            }
            CampaignFaultKind::Custom => {
                let service_name = candidate.service.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign custom fault requires service".to_owned())
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign custom fault requires after".to_owned())
                })?;
                let command = candidate.command.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign custom fault requires command".to_owned())
                })?;
                if command.is_empty()
                    || command
                        .iter()
                        .any(|entry| entry.is_empty() || entry.contains('\0'))
                {
                    return Err(ComposeError::Invalid(
                        "campaign custom fault command must be a non-empty argv with no empty or NUL arguments"
                            .to_owned(),
                    ));
                }
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                let service = services.get_mut(service_name).ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "campaign custom fault references unknown service {service_name:?}"
                    ))
                })?;
                let Some(contract) = service.run.container_service.as_mut() else {
                    return Err(ComposeError::Invalid(format!(
                        "campaign custom fault requires image-backed service {service_name:?} with a container_service contract"
                    )));
                };
                contract.campaign = true;
                if candidate.network.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign custom faults accept only service, after, and command".to_owned(),
                    ));
                }
                let duplicate = faults.iter().any(|fault| {
                    fault.kind == CampaignFaultKind::Custom
                        && fault.service.as_deref() == Some(service_name)
                        && fault.after == after
                        && fault.after_input == after_input
                        && fault.command.as_deref() == Some(command)
                });
                if duplicate {
                    if generated_by_profile {
                        // A generated candidate that restates a fault the
                        // user already declared (or an earlier candidate
                        // derived from the same command) is redundant, not
                        // an authoring mistake.
                        continue;
                    }
                    return Err(ComposeError::Invalid(format!(
                        "campaign duplicates a custom fault for service {service_name:?} at {after:?}"
                    )));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: None,
                    after,
                    after_input,
                    command: Some(command.to_owned()),
                    until,
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
                    rate: None,
                });
            }
            CampaignFaultKind::Partition | CampaignFaultKind::Heal => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign partition/heal action requires network".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign partition/heal action requires after".to_owned(),
                    )
                })?;
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                if !services
                    .values()
                    .any(|service| service.networks.iter().any(|name| name == network))
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action references unknown network {network:?}",
                    )));
                }
                if candidate.service.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign partition/heal actions accept only network and after".to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: None,
                    network: Some(network.to_owned()),
                    from: None,
                    to: None,
                    drive: None,
                    after,
                    after_input,
                    command: None,
                    until,
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
                    rate: None,
                });
            }
            CampaignFaultKind::LinkPartition | CampaignFaultKind::LinkHeal => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_partition/link_heal action requires network".to_owned(),
                    )
                })?;
                let from = candidate.from.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_partition/link_heal action requires from".to_owned(),
                    )
                })?;
                let to = candidate.to.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_partition/link_heal action requires to".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_partition/link_heal action requires after".to_owned(),
                    )
                })?;
                if from == to {
                    return Err(ComposeError::Invalid(
                        "campaign directed link action requires distinct from and to services"
                            .to_owned(),
                    ));
                }
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                for service_name in [from, to] {
                    let service = services.get(service_name).ok_or_else(|| {
                        ComposeError::Invalid(format!(
                            "campaign directed link action references unknown service {service_name:?}",
                        ))
                    })?;
                    if !service.networks.iter().any(|name| name == network) {
                        return Err(ComposeError::Invalid(format!(
                            "campaign directed link action service {service_name:?} is not on network {network:?}",
                        )));
                    }
                }
                if candidate.service.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign link_partition/link_heal actions accept only network, from, to, and after"
                            .to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: None,
                    network: Some(network.to_owned()),
                    from: Some(from.to_owned()),
                    to: Some(to.to_owned()),
                    drive: None,
                    after,
                    after_input,
                    command: None,
                    until,
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
                    rate: None,
                });
            }
            CampaignFaultKind::CpuThrottle | CampaignFaultKind::CpuRelease => {
                let service_name = candidate.service.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign cpu_throttle/cpu_release action requires service".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign cpu_throttle/cpu_release action requires after".to_owned(),
                    )
                })?;
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                if !services.contains_key(service_name) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign cpu_throttle/cpu_release action references unknown service {service_name:?}"
                    )));
                }
                if candidate.network.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign cpu_throttle/cpu_release actions accept only service, after, duration_rounds, and every_n_rounds"
                            .to_owned(),
                    ));
                }
                if let Some(every_n) = candidate.every_n_rounds {
                    if every_n < 2 || every_n > 64 {
                        return Err(ComposeError::Invalid(
                            "campaign cpu_throttle every_n_rounds must be between 2 and 64"
                                .to_owned(),
                        ));
                    }
                }
                if matches!(candidate.kind, CampaignFaultKind::CpuThrottle) {
                    let duration = candidate.duration_rounds.ok_or_else(|| {
                        ComposeError::Invalid(
                            "campaign cpu_throttle requires duration_rounds".to_owned(),
                        )
                    })?;
                    if duration == 0 || duration > 100_000 {
                        return Err(ComposeError::Invalid(
                            "campaign cpu_throttle duration_rounds must be between 1 and 100000"
                                .to_owned(),
                        ));
                    }
                    if candidate.every_n_rounds.is_none() {
                        return Err(ComposeError::Invalid(
                            "campaign cpu_throttle requires every_n_rounds".to_owned(),
                        ));
                    }
                } else if candidate.duration_rounds.is_some() || candidate.every_n_rounds.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign cpu_release accepts only service and after".to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: None,
                    after,
                    after_input,
                    command: None,
                    until,
                    at_round: None,
                    duration_rounds: candidate.duration_rounds,
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
                    every_n_rounds: candidate.every_n_rounds,
                    rate: None,
                });
            }
            CampaignFaultKind::ClockRate | CampaignFaultKind::ClockRateRelease => {
                let service_name = candidate.service.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign clock_rate/clock_rate_release action requires service".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign clock_rate/clock_rate_release action requires after".to_owned(),
                    )
                })?;
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                let has_virtual_time = services
                    .get(service_name)
                    .map(|service| service.run.run.virtual_time.is_some())
                    .ok_or_else(|| {
                        ComposeError::Invalid(format!(
                            "campaign clock_rate/clock_rate_release action references unknown service {service_name:?}"
                        ))
                    })?;
                if !has_virtual_time {
                    return Err(ComposeError::Invalid(format!(
                        "campaign clock_rate/clock_rate_release action requires virtual_time on service {service_name:?}"
                    )));
                }
                if candidate.network.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                    || candidate.every_n_rounds.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign clock_rate/clock_rate_release actions accept only service, after, rate, and duration_rounds"
                            .to_owned(),
                    ));
                }
                if let Some(rate) = candidate.rate {
                    if !(2..=16).contains(&rate) {
                        return Err(ComposeError::Invalid(
                            "campaign clock_rate rate must be between 2 and 16".to_owned(),
                        ));
                    }
                }
                if matches!(candidate.kind, CampaignFaultKind::ClockRate) {
                    let duration = candidate.duration_rounds.ok_or_else(|| {
                        ComposeError::Invalid(
                            "campaign clock_rate requires duration_rounds".to_owned(),
                        )
                    })?;
                    if duration == 0 || duration > 100_000 {
                        return Err(ComposeError::Invalid(
                            "campaign clock_rate duration_rounds must be between 1 and 100000"
                                .to_owned(),
                        ));
                    }
                    if candidate.rate.is_none() {
                        return Err(ComposeError::Invalid(
                            "campaign clock_rate requires rate".to_owned(),
                        ));
                    }
                } else if candidate.duration_rounds.is_some() || candidate.rate.is_some() {
                    return Err(ComposeError::Invalid(
                        "campaign clock_rate_release accepts only service and after".to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: None,
                    after,
                    after_input,
                    command: None,
                    until,
                    at_round: None,
                    duration_rounds: candidate.duration_rounds,
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
                    rate: candidate.rate,
                });
            }
            CampaignFaultKind::LinkClog | CampaignFaultKind::LinkUnclog => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_clog/link_unclog action requires network".to_owned(),
                    )
                })?;
                let from = candidate.from.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_clog/link_unclog action requires from".to_owned(),
                    )
                })?;
                let to = candidate.to.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_clog/link_unclog action requires to".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_clog/link_unclog action requires after".to_owned(),
                    )
                })?;
                if from == to {
                    return Err(ComposeError::Invalid(
                        "campaign directed link action requires distinct from and to services"
                            .to_owned(),
                    ));
                }
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                for service_name in [from, to] {
                    let service = services.get(service_name).ok_or_else(|| {
                        ComposeError::Invalid(format!(
                            "campaign directed link action references unknown service {service_name:?}",
                        ))
                    })?;
                    if !service.networks.iter().any(|name| name == network) {
                        return Err(ComposeError::Invalid(format!(
                            "campaign directed link action service {service_name:?} is not on network {network:?}",
                        )));
                    }
                }
                if candidate.service.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign link_clog/link_unclog actions accept only network, from, to, after, and latency_rounds"
                            .to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::LinkClog) {
                    let latency = candidate.latency_rounds.ok_or_else(|| {
                        ComposeError::Invalid(
                            "campaign link_clog requires latency_rounds".to_owned(),
                        )
                    })?;
                    if latency == 0 || latency > 4096 {
                        return Err(ComposeError::Invalid(
                            "campaign link_clog latency_rounds must be between 1 and 4096"
                                .to_owned(),
                        ));
                    }
                } else if candidate.latency_rounds.is_some() {
                    return Err(ComposeError::Invalid(
                        "campaign link_unclog accepts only network, from, to, and after".to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: None,
                    network: Some(network.to_owned()),
                    from: Some(from.to_owned()),
                    to: Some(to.to_owned()),
                    drive: None,
                    after,
                    after_input,
                    command: None,
                    until,
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: None,
                    latency_rounds: candidate.latency_rounds,
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
                    rate: None,
                });
            }
            CampaignFaultKind::StorageFault | CampaignFaultKind::StorageRecover => {
                let service_name = candidate.service.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign storage_fault action requires service".to_owned(),
                    )
                })?;
                let drive = candidate.drive.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign storage_fault action requires drive".to_owned())
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign storage_fault action requires after".to_owned())
                })?;
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                let service = services.get(service_name).ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "campaign storage_fault references unknown service {service_name:?}",
                    ))
                })?;
                if !service
                    .run
                    .storage
                    .iter()
                    .any(|storage| storage.id == drive)
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign storage_fault references unknown drive {drive:?} on service {service_name:?}",
                    )));
                }
                let error_ppm = candidate.error_ppm.unwrap_or(0);
                if error_ppm > 1_000_000 {
                    return Err(ComposeError::Invalid(
                        "campaign storage_fault error_ppm must be at most 1000000".to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::StorageFault)
                    && error_ppm == 0
                    && candidate.latency_rounds.unwrap_or(0) == 0
                    && candidate.torn_write_bytes.is_none()
                    && candidate.corrupt_read_xor.is_none()
                {
                    return Err(ComposeError::Invalid(
                        "campaign storage_fault must set error_ppm, latency_rounds, torn_write_bytes, or corrupt_read_xor"
                            .to_owned(),
                    ));
                }
                if candidate.network.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || has_network_conditions
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign storage_fault accepts service, drive, after, error_ppm, latency_rounds, torn_write_bytes, and corrupt_read_xor"
                            .to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::StorageRecover)
                    && (candidate.error_ppm.is_some()
                        || candidate.latency_rounds.is_some()
                        || candidate.torn_write_bytes.is_some()
                        || candidate.corrupt_read_xor.is_some())
                {
                    return Err(ComposeError::Invalid(
                        "campaign storage_recover accepts only service, drive, and after"
                            .to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: Some(drive.to_owned()),
                    after,
                    after_input,
                    command: None,
                    until,
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: matches!(candidate.kind, CampaignFaultKind::StorageFault)
                        .then_some(error_ppm),
                    latency_rounds: candidate.latency_rounds,
                    torn_write_bytes: candidate.torn_write_bytes,
                    corrupt_read_xor: candidate.corrupt_read_xor,
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
                    rate: None,
                });
            }
            CampaignFaultKind::NetworkFault
            | CampaignFaultKind::NetworkRecover
            | CampaignFaultKind::LinkFault
            | CampaignFaultKind::LinkRecover => {
                let directed = matches!(
                    candidate.kind,
                    CampaignFaultKind::LinkFault | CampaignFaultKind::LinkRecover
                );
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign network/link fault action requires network".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign network/link fault action requires after".to_owned(),
                    )
                })?;
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                if !services
                    .values()
                    .any(|service| service.networks.iter().any(|name| name == network))
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action references unknown network {network:?}",
                    )));
                }
                let endpoints = match (candidate.from.as_deref(), candidate.to.as_deref()) {
                    (Some(from), Some(to)) if directed && from != to => {
                        for service_name in [from, to] {
                            let service = services.get(service_name).ok_or_else(|| {
                                ComposeError::Invalid(format!(
                                    "campaign directed link action references unknown service {service_name:?}"
                                ))
                            })?;
                            if !service.networks.iter().any(|name| name == network) {
                                return Err(ComposeError::Invalid(format!(
                                    "campaign directed link action service {service_name:?} is not on network {network:?}"
                                )));
                            }
                        }
                        Some((from, to))
                    }
                    (None, None) if !directed => None,
                    _ => {
                        return Err(ComposeError::Invalid(
                            "link_fault/link_recover require both distinct from and to services; network_fault/network_recover do not accept them"
                                .to_owned(),
                        ));
                    }
                };
                if candidate.service.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || candidate.command.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign network/link fault actions accept network, after, optional directed from/to, and packet-condition fields"
                            .to_owned(),
                    ));
                }
                if matches!(
                    candidate.kind,
                    CampaignFaultKind::NetworkFault | CampaignFaultKind::LinkFault
                ) && !has_network_conditions
                    && candidate.latency_rounds.is_none()
                {
                    return Err(ComposeError::Invalid(
                        "campaign network_fault must set one packet-condition field".to_owned(),
                    ));
                }
                if matches!(
                    candidate.kind,
                    CampaignFaultKind::NetworkRecover | CampaignFaultKind::LinkRecover
                ) && (has_network_conditions || candidate.latency_rounds.is_some())
                {
                    return Err(ComposeError::Invalid(
                        "campaign network_recover accepts only network and after".to_owned(),
                    ));
                }
                for (name, value) in [
                    ("drop_ppm", candidate.drop_ppm),
                    ("duplicate_ppm", candidate.duplicate_ppm),
                    ("corrupt_ppm", candidate.corrupt_ppm),
                ] {
                    if value.is_some_and(|value| value > 1_000_000) {
                        return Err(ComposeError::Invalid(format!(
                            "campaign network_fault {name} must be at most 1000000"
                        )));
                    }
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: None,
                    network: Some(network.to_owned()),
                    from: endpoints.map(|(from, _)| from.to_owned()),
                    to: endpoints.map(|(_, to)| to.to_owned()),
                    drive: None,
                    after,
                    after_input,
                    command: None,
                    until,
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: None,
                    latency_rounds: candidate.latency_rounds,
                    torn_write_bytes: None,
                    corrupt_read_xor: None,
                    ethertype: None,
                    ip_protocol: None,
                    source_port: None,
                    destination_port: None,
                    drop_ppm: candidate.drop_ppm,
                    duplicate_ppm: candidate.duplicate_ppm,
                    corrupt_ppm: candidate.corrupt_ppm,
                    jitter_rounds: candidate.jitter_rounds,
                    tx_bytes_per_round: candidate.tx_bytes_per_round,
                    mtu_bytes: candidate.mtu_bytes,
                    tx_queue_frames: candidate.tx_queue_frames,
                    rx_queue_frames: candidate.rx_queue_frames,
                    every_n_rounds: None,
                    rate: None,
                });
            }
            CampaignFaultKind::PacketFault | CampaignFaultKind::PacketRecover => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign packet_fault/packet_recover action requires network".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign packet_fault/packet_recover action requires after".to_owned(),
                    )
                })?;
                let ethertype = candidate.ethertype.ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign packet_fault/packet_recover action requires ethertype".to_owned(),
                    )
                })?;
                if ethertype < 0x0600 {
                    return Err(ComposeError::Invalid(
                        "campaign packet_fault ethertype must be an Ethernet EtherType (at least 0x0600)"
                            .to_owned(),
                    ));
                }
                if candidate.ip_protocol.is_some() && !matches!(ethertype, 0x0800 | 0x86dd) {
                    return Err(ComposeError::Invalid(
                        "campaign packet protocol selectors require IPv4 (0x0800) or IPv6 (0x86dd) ethertype"
                            .to_owned(),
                    ));
                }
                if (candidate.source_port.is_some() || candidate.destination_port.is_some())
                    && !matches!(candidate.ip_protocol, Some(6 | 17))
                {
                    return Err(ComposeError::Invalid(
                        "campaign packet port selectors require ip_protocol 6 (TCP) or 17 (UDP)"
                            .to_owned(),
                    ));
                }
                let (after, after_input) = normalize_campaign_fault_after(after, &operations)?;
                if !services
                    .values()
                    .any(|service| service.networks.iter().any(|name| name == network))
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action references unknown network {network:?}",
                    )));
                }
                let directed = match (candidate.from.as_deref(), candidate.to.as_deref()) {
                    (None, None) => None,
                    (Some(from), Some(to)) if from != to => {
                        for service_name in [from, to] {
                            let service = services.get(service_name).ok_or_else(|| {
                                ComposeError::Invalid(format!(
                                    "campaign packet action references unknown service {service_name:?}",
                                ))
                            })?;
                            if !service.networks.iter().any(|name| name == network) {
                                return Err(ComposeError::Invalid(format!(
                                    "campaign packet action service {service_name:?} is not on network {network:?}",
                                )));
                            }
                        }
                        Some((from, to))
                    }
                    _ => {
                        return Err(ComposeError::Invalid(
                            "campaign packet action requires both distinct from and to services"
                                .to_owned(),
                        ));
                    }
                };
                if candidate.service.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.duplicate_ppm.is_some()
                    || candidate.corrupt_ppm.is_some()
                    || candidate.jitter_rounds.is_some()
                    || candidate.tx_bytes_per_round.is_some()
                    || candidate.mtu_bytes.is_some()
                    || candidate.tx_queue_frames.is_some()
                    || candidate.rx_queue_frames.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign packet_fault/packet_recover actions accept network, after, ethertype, drop_ppm, and optional from/to"
                            .to_owned(),
                    ));
                }
                let drop_ppm = candidate.drop_ppm.unwrap_or(0);
                if drop_ppm > 1_000_000 {
                    return Err(ComposeError::Invalid(
                        "campaign packet_fault drop_ppm must be at most 1000000".to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::PacketFault)
                    && candidate.drop_ppm.is_none()
                {
                    return Err(ComposeError::Invalid(
                        "campaign packet_fault requires drop_ppm".to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::PacketRecover)
                    && candidate.drop_ppm.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign packet_recover accepts only network, after, and ethertype"
                            .to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    required: candidate.required,
                    service: None,
                    network: Some(network.to_owned()),
                    from: directed.map(|(from, _)| from.to_owned()),
                    to: directed.map(|(_, to)| to.to_owned()),
                    drive: None,
                    after,
                    after_input,
                    command: None,
                    until,
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: None,
                    latency_rounds: None,
                    torn_write_bytes: None,
                    corrupt_read_xor: None,
                    ethertype: Some(ethertype),
                    ip_protocol: candidate.ip_protocol,
                    source_port: candidate.source_port,
                    destination_port: candidate.destination_port,
                    drop_ppm: matches!(candidate.kind, CampaignFaultKind::PacketFault)
                        .then_some(drop_ppm),
                    duplicate_ppm: None,
                    corrupt_ppm: None,
                    jitter_rounds: None,
                    tx_bytes_per_round: None,
                    mtu_bytes: None,
                    tx_queue_frames: None,
                    rx_queue_frames: None,
                    every_n_rounds: None,
                    rate: None,
                });
            }
        }
    }
    let required_faults = faults.iter().filter(|fault| fault.required).count();
    if required_faults > usize::from(campaign.max_faults_per_run) {
        return Err(ComposeError::Invalid(format!(
            "campaign declares {required_faults} required faults but max_faults_per_run is {}",
            campaign.max_faults_per_run
        )));
    }
    let mut quiet = Vec::with_capacity(campaign.quiet.len());
    for window in campaign.quiet {
        if quiet.len() >= 8 {
            return Err(ComposeError::Invalid(
                "campaign declares more than 8 quiet windows".to_owned(),
            ));
        }
        let (before, before_input) = normalize_campaign_fault_after(&window.before, &operations)?;
        if before_input.is_some() || before.is_none() {
            return Err(ComposeError::Invalid(
                "campaign quiet windows accept only an operation name as before".to_owned(),
            ));
        }
        let before = before.expect("validated quiet barrier");
        if quiet
            .iter()
            .any(|existing: &QuietWindowPlan| existing.before == before)
        {
            return Err(ComposeError::Invalid(format!(
                "campaign declares the same quiet window twice: before {before:?}"
            )));
        }
        quiet.push(QuietWindowPlan { before });
    }
    let mut property_names = BTreeSet::new();
    let mut properties = Vec::with_capacity(campaign.properties.len());
    for property in campaign.properties {
        validate_name("campaign property", &property.name)?;
        if !property_names.insert(property.name.clone()) {
            return Err(ComposeError::Invalid(format!(
                "campaign property {:?} is declared more than once",
                property.name
            )));
        }
        if property.contains.as_ref().is_some_and(String::is_empty) {
            return Err(ComposeError::Invalid(format!(
                "campaign property {:?} has an empty contains value",
                property.name
            )));
        }
        if property.contains.is_none()
            && property.predicate.is_none()
            && property.requires_serial_all.is_none()
            && property.requires_serial_any.is_none()
            && property.excludes_serial_any.is_none()
            && property.requires_serial_correlations.is_none()
            && property.requires_serial_joins.is_none()
            && property.requires_serial_evidence.is_none()
            && property.excludes_serial_evidence.is_none()
        {
            return Err(ComposeError::Invalid(format!(
                "campaign property {:?} needs serial evidence",
                property.name
            )));
        }
        if property.contains.is_none()
            && (!property.contains_all.is_empty()
                || !property.contains_any.is_empty()
                || !property.contains_none.is_empty())
        {
            return Err(ComposeError::Invalid(format!(
                "campaign property {:?} needs contains before compound predicates",
                property.name
            )));
        }
        if property.contains_all.iter().any(String::is_empty)
            || property.contains_any.iter().any(String::is_empty)
            || property.contains_none.iter().any(String::is_empty)
        {
            return Err(ComposeError::Invalid(format!(
                "campaign property {:?} has an empty compound predicate",
                property.name
            )));
        }
        if let Some(service) = &property.service {
            if !services.contains_key(service) {
                return Err(ComposeError::Invalid(format!(
                    "campaign property {:?} references unknown service {service:?}",
                    property.name
                )));
            }
        }
        let predicate = property
            .predicate
            .map(|predicate| {
                normalize_serial_predicate(predicate, &format!("property {:?}", property.name))
            })
            .transpose()?;
        let context = format!("property {:?}", property.name);
        let requires_serial_all = normalize_operation_serial_guards(
            property.requires_serial_all,
            "requires_serial_all",
            &context,
            services,
        )?;
        let requires_serial_any = normalize_operation_serial_guards(
            property.requires_serial_any,
            "requires_serial_any",
            &context,
            services,
        )?;
        let excludes_serial_any = normalize_operation_serial_guards(
            property.excludes_serial_any,
            "excludes_serial_any",
            &context,
            services,
        )?;
        let requires_serial_correlations = normalize_serial_correlations(
            property.requires_serial_correlations,
            &context,
            services,
        )?;
        let requires_serial_joins =
            normalize_serial_joins(property.requires_serial_joins, &context, services)?;
        let requires_serial_evidence = property
            .requires_serial_evidence
            .map(|evidence| {
                normalize_serial_evidence_root(
                    evidence,
                    &context,
                    services,
                    &evidence_definitions,
                    &mut resolved_evidence,
                )
            })
            .transpose()?;
        let excludes_serial_evidence = property
            .excludes_serial_evidence
            .map(|evidence| {
                normalize_serial_evidence_root(
                    evidence,
                    &context,
                    services,
                    &evidence_definitions,
                    &mut resolved_evidence,
                )
            })
            .transpose()?;
        properties.push(PropertyPlan {
            name: property.name,
            kind: property.kind,
            contains: property.contains,
            contains_all: property.contains_all,
            contains_any: property.contains_any,
            contains_none: property.contains_none,
            predicate,
            requires_serial_all,
            requires_serial_any,
            excludes_serial_any,
            requires_serial_correlations,
            requires_serial_joins,
            requires_serial_evidence,
            excludes_serial_evidence,
            service: property.service,
        });
    }
    Ok(Some(CampaignPlan {
        driver: campaign.driver,
        fault_profile: campaign.fault_profile,
        test_template: (selected_templates.len() == 1).then(|| selected_templates[0].clone()),
        test_templates: (selected_templates.len() > 1)
            .then_some(selected_templates)
            .unwrap_or_default(),
        max_parallel_commands: campaign.max_parallel_commands,
        guidance: campaign.guidance,
        coverage: campaign.coverage,
        state: initial_state,
        operations,
        stages: campaign.stages,
        faults,
        quiet,
        shard: None,
        properties,
        max_runs: campaign.max_runs,
        max_faults_per_run: campaign.max_faults_per_run,
        max_operations_per_run: campaign.max_operations_per_run,
    }))
}

/// Build a useful failure catalog from facts already locked in the plan. The
/// cap is deliberately applied while expanding each stable, sorted topology so
/// a wide Compose file cannot turn one profile into an unbounded search input.
/// Whether an operation is an ordinary work boundary the standard fault
/// profile may target: never setup, completion, assertion, or recovery
/// phases, and never the lifecycle-protected first, eventually, or finally
/// commands.
fn profile_eligible_operation(operation: &OperationPlan) -> bool {
    !matches!(
        operation.command,
        Some(
            ComposeTestCommand::First
                | ComposeTestCommand::Eventually
                | ComposeTestCommand::Finally
        )
    ) && !matches!(
        operation.shell_phase,
        Some(
            ComposeShellPhase::Setup
                | ComposeShellPhase::Completion
                | ComposeShellPhase::Assertion
                | ComposeShellPhase::Recovery
        )
    )
}

fn standard_fault_profile(
    operations: &[OperationPlan],
    services: &mut BTreeMap<String, ComposeServicePlan>,
) -> Vec<ComposeCampaignFault> {
    const MAX_PROFILE_CANDIDATES: usize = 512;
    let boundaries = operations
        .iter()
        .filter(|operation| profile_eligible_operation(operation))
        .map(|operation| operation.name.clone())
        .collect::<Vec<_>>();
    let lifecycle_services = services
        .iter_mut()
        .filter_map(|(name, service)| {
            service.run.container_service.as_mut().map(|contract| {
                contract.campaign = true;
                name.clone()
            })
        })
        .collect::<Vec<_>>();
    let mut network_services = BTreeMap::<String, Vec<String>>::new();
    for (name, service) in services.iter() {
        for network in &service.networks {
            network_services
                .entry(network.clone())
                .or_default()
                .push(name.clone());
        }
    }
    let mut generated = Vec::new();
    for after in &boundaries {
        for service in &lifecycle_services {
            for kind in [
                CampaignFaultKind::ServiceStop,
                CampaignFaultKind::ServiceKill,
                CampaignFaultKind::ServiceRestart,
            ] {
                let mut fault = empty_campaign_fault(kind);
                fault.service = Some(service.clone());
                fault.after = Some(after.clone());
                generated.push(fault);
                if generated.len() == MAX_PROFILE_CANDIDATES {
                    return generated;
                }
            }
            let mut throttled = empty_campaign_fault(CampaignFaultKind::CpuThrottle);
            throttled.service = Some(service.clone());
            throttled.after = Some(after.clone());
            throttled.duration_rounds = Some(16);
            throttled.every_n_rounds = Some(4);
            generated.push(throttled);
            if generated.len() == MAX_PROFILE_CANDIDATES {
                return generated;
            }
            let has_virtual_time = services
                .get(service)
                .is_some_and(|entry| entry.run.run.virtual_time.is_some());
            if has_virtual_time {
                let mut clock_rate = empty_campaign_fault(CampaignFaultKind::ClockRate);
                clock_rate.service = Some(service.clone());
                clock_rate.after = Some(after.clone());
                clock_rate.duration_rounds = Some(32);
                clock_rate.rate = Some(4);
                generated.push(clock_rate);
                if generated.len() == MAX_PROFILE_CANDIDATES {
                    return generated;
                }
            }
        }
        for (network, endpoints) in &network_services {
            for from in endpoints {
                for to in endpoints.iter().filter(|to| *to != from) {
                    let mut partition = empty_campaign_fault(CampaignFaultKind::LinkPartition);
                    partition.network = Some(network.clone());
                    partition.from = Some(from.clone());
                    partition.to = Some(to.clone());
                    partition.after = Some(after.clone());
                    generated.push(partition);
                    if generated.len() == MAX_PROFILE_CANDIDATES {
                        return generated;
                    }

                    let mut clogged = empty_campaign_fault(CampaignFaultKind::LinkClog);
                    clogged.network = Some(network.clone());
                    clogged.from = Some(from.clone());
                    clogged.to = Some(to.clone());
                    clogged.after = Some(after.clone());
                    clogged.latency_rounds = Some(64);
                    generated.push(clogged);
                    if generated.len() == MAX_PROFILE_CANDIDATES {
                        return generated;
                    }

                    let mut degraded = empty_campaign_fault(CampaignFaultKind::LinkFault);
                    degraded.network = Some(network.clone());
                    degraded.from = Some(from.clone());
                    degraded.to = Some(to.clone());
                    degraded.after = Some(after.clone());
                    degraded.drop_ppm = Some(100_000);
                    degraded.duplicate_ppm = Some(10_000);
                    degraded.corrupt_ppm = Some(1_000);
                    degraded.latency_rounds = Some(2);
                    degraded.jitter_rounds = Some(2);
                    degraded.tx_bytes_per_round = Some(4_096);
                    degraded.mtu_bytes = Some(1_200);
                    degraded.tx_queue_frames = Some(8);
                    degraded.rx_queue_frames = Some(8);
                    generated.push(degraded);
                    if generated.len() == MAX_PROFILE_CANDIDATES {
                        return generated;
                    }
                }
            }
        }
    }
    // Custom candidates re-run a service's own declared commands at eligible
    // barriers - the classic duplicate-delivery and concurrent-invocation
    // faults - so exploration exercises user commands without hand-declaring
    // every fault. Sources are the eligible shell argvs of image-backed
    // services; the argv reaches execve unchanged, and like every custom
    // fault a generated candidate records completion without restoring.
    let mut declared_commands: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    for operation in operations {
        if !profile_eligible_operation(operation) {
            continue;
        }
        let (Some(service), Some(command)) = (
            Some(operation.service.as_str()),
            operation.shell_command.as_deref(),
        ) else {
            continue;
        };
        if !lifecycle_services.iter().any(|name| name == service) {
            continue;
        }
        let commands = declared_commands.entry(service.to_owned()).or_default();
        if !commands.iter().any(|candidate| candidate == command) {
            commands.push(command.to_owned());
        }
    }
    for (service, commands) in &declared_commands {
        for command in commands {
            for after in &boundaries {
                let mut fault = empty_campaign_fault(CampaignFaultKind::Custom);
                fault.service = Some(service.clone());
                fault.after = Some(after.clone());
                fault.command = Some(command.clone());
                generated.push(fault);
                if generated.len() == MAX_PROFILE_CANDIDATES {
                    return generated;
                }
            }
        }
    }
    generated
}

fn normalize_operation_serial_guards(
    guards: Option<Vec<ComposeOperationSerialGuard>>,
    field: &str,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<Vec<OperationSerialGuardPlan>, ComposeError> {
    let Some(guards) = guards else {
        return Ok(Vec::new());
    };
    if guards.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has an empty {field} guard set"
        )));
    }
    guards
        .into_iter()
        .map(|guard| normalize_operation_serial_guard(guard, context, services))
        .collect()
}

fn normalize_operation_serial_guard(
    guard: ComposeOperationSerialGuard,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<OperationSerialGuardPlan, ComposeError> {
    if let Some(service) = &guard.service {
        if !services.contains_key(service) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} references unknown serial-guard service {service:?}"
            )));
        }
    }
    Ok(OperationSerialGuardPlan {
        service: guard.service.clone(),
        predicate: normalize_serial_predicate(guard.into_predicate(), context)?,
    })
}

fn normalize_serial_correlations(
    correlations: Option<Vec<ComposeSerialCorrelation>>,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<Vec<SerialCorrelationPlan>, ComposeError> {
    let Some(correlations) = correlations else {
        return Ok(Vec::new());
    };
    if correlations.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has an empty requires_serial_correlations set"
        )));
    }
    correlations
        .into_iter()
        .map(|correlation| {
            Ok(SerialCorrelationPlan {
                capture: normalize_json_correlation_endpoint(
                    correlation.capture,
                    "capture",
                    context,
                    services,
                )?,
                equals: normalize_json_correlation_endpoint(
                    correlation.equals,
                    "equals",
                    context,
                    services,
                )?,
            })
        })
        .collect()
}

fn normalize_json_correlation_endpoint(
    endpoint: ComposeJsonCorrelationEndpoint,
    role: &str,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<JsonCorrelationEndpointPlan, ComposeError> {
    if let Some(service) = &endpoint.service {
        if !services.contains_key(service) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} {role} correlation references unknown service {service:?}"
            )));
        }
    }
    if endpoint.pointer.is_some() && !endpoint.pointers.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} {role} correlation needs pointer or pointers, not both"
        )));
    }
    let pointers = endpoint
        .pointer
        .into_iter()
        .chain(endpoint.pointers)
        .collect::<Vec<_>>();
    if pointers.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} {role} correlation needs a JSON pointer"
        )));
    }
    let mut unique_pointers = BTreeSet::new();
    for pointer in &pointers {
        if !valid_json_pointer(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} has invalid {role} correlation JSON pointer {pointer:?}"
            )));
        }
        if !unique_pointers.insert(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} repeats {role} correlation JSON pointer {pointer:?}"
            )));
        }
    }
    Ok(JsonCorrelationEndpointPlan {
        service: endpoint.service,
        pointers,
        json: normalize_json_predicate(endpoint.json, context, false)?,
    })
}

fn normalize_serial_joins(
    joins: Option<Vec<ComposeSerialJoin>>,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<Vec<SerialJoinPlan>, ComposeError> {
    let Some(joins) = joins else {
        return Ok(Vec::new());
    };
    if joins.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has an empty requires_serial_joins set"
        )));
    }
    joins
        .into_iter()
        .map(|join| {
            if join.endpoints.len() < 2 {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} serial join needs at least two endpoints"
                )));
            }
            Ok(SerialJoinPlan {
                endpoints: join
                    .endpoints
                    .into_iter()
                    .map(|endpoint| {
                        normalize_json_correlation_endpoint(endpoint, "join", context, services)
                    })
                    .collect::<Result<_, _>>()?,
                quantifier: join.quantifier,
                occurs: join
                    .occurs
                    .map(|occurs| normalize_serial_match_count(occurs, context))
                    .transpose()?,
            })
        })
        .collect()
}

fn normalize_serial_evidence_definitions(
    definitions: &BTreeMap<String, ComposeSerialEvidence>,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<BTreeMap<String, SerialEvidencePlan>, ComposeError> {
    for name in definitions.keys() {
        validate_name("campaign serial evidence", name)?;
    }
    let mut resolved = BTreeMap::new();
    for name in definitions.keys() {
        resolve_named_serial_evidence(
            name,
            definitions,
            services,
            &mut resolved,
            &mut BTreeSet::new(),
        )?;
    }
    Ok(resolved)
}

fn normalize_serial_evidence_root(
    evidence: ComposeSerialEvidence,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
    definitions: &BTreeMap<String, ComposeSerialEvidence>,
    resolved: &mut BTreeMap<String, SerialEvidencePlan>,
) -> Result<SerialEvidencePlan, ComposeError> {
    normalize_serial_evidence(
        evidence,
        context,
        services,
        definitions,
        resolved,
        &mut BTreeSet::new(),
    )
}

fn resolve_named_serial_evidence(
    name: &str,
    definitions: &BTreeMap<String, ComposeSerialEvidence>,
    services: &BTreeMap<String, ComposeServicePlan>,
    resolved: &mut BTreeMap<String, SerialEvidencePlan>,
    resolving: &mut BTreeSet<String>,
) -> Result<SerialEvidencePlan, ComposeError> {
    if let Some(evidence) = resolved.get(name) {
        return Ok(evidence.clone());
    }
    if !resolving.insert(name.to_owned()) {
        return Err(ComposeError::Invalid(format!(
            "campaign serial evidence {name:?} is cyclic"
        )));
    }
    let evidence = definitions.get(name).cloned().ok_or_else(|| {
        ComposeError::Invalid(format!(
            "campaign references unknown serial evidence {name:?}"
        ))
    })?;
    let normalized = normalize_serial_evidence(
        evidence,
        &format!("serial evidence {name:?}"),
        services,
        definitions,
        resolved,
        resolving,
    )?;
    resolving.remove(name);
    resolved.insert(name.to_owned(), normalized.clone());
    Ok(normalized)
}

fn normalize_serial_evidence(
    evidence: ComposeSerialEvidence,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
    definitions: &BTreeMap<String, ComposeSerialEvidence>,
    resolved: &mut BTreeMap<String, SerialEvidencePlan>,
    resolving: &mut BTreeSet<String>,
) -> Result<SerialEvidencePlan, ComposeError> {
    let choices = [
        !evidence.all.is_empty(),
        !evidence.any.is_empty(),
        !evidence.none.is_empty(),
        evidence.guard.is_some(),
        evidence.correlation.is_some(),
        evidence.join.is_some(),
        evidence.relation.is_some(),
        evidence.path.is_some(),
        evidence.workflow.is_some(),
        evidence.use_.is_some(),
    ];
    if choices.into_iter().filter(|choice| *choice).count() != 1 {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} serial evidence needs exactly one of all, any, none, guard, correlation, join, relation, path, workflow, or use"
        )));
    }
    let all = evidence
        .all
        .into_iter()
        .map(|child| {
            normalize_serial_evidence(child, context, services, definitions, resolved, resolving)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let any = evidence
        .any
        .into_iter()
        .map(|child| {
            normalize_serial_evidence(child, context, services, definitions, resolved, resolving)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let none = evidence
        .none
        .into_iter()
        .map(|child| {
            normalize_serial_evidence(child, context, services, definitions, resolved, resolving)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let guard = evidence
        .guard
        .map(|guard| normalize_operation_serial_guard(guard, context, services))
        .transpose()?;
    let correlation = evidence
        .correlation
        .map(|correlation| {
            normalize_serial_correlations(Some(vec![correlation]), context, services)
                .map(|mut correlations| correlations.remove(0))
        })
        .transpose()?;
    let join = evidence
        .join
        .map(|join| {
            normalize_serial_joins(Some(vec![join]), context, services)
                .map(|mut joins| joins.remove(0))
        })
        .transpose()?;
    let relation = evidence
        .relation
        .map(|relation| normalize_serial_relation(relation, context, services))
        .transpose()?;
    let path = evidence
        .path
        .map(|path| normalize_serial_path(path, context, services))
        .transpose()?;
    let workflow = evidence
        .workflow
        .map(|workflow| normalize_serial_workflow(workflow, context, services))
        .transpose()?;
    if let Some(name) = evidence.use_ {
        return resolve_named_serial_evidence(&name, definitions, services, resolved, resolving);
    }
    Ok(SerialEvidencePlan {
        all,
        any,
        none,
        guard,
        correlation,
        join,
        relation,
        path,
        workflow,
    })
}

fn normalize_serial_relation(
    relation: ComposeSerialRelation,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<SerialRelationPlan, ComposeError> {
    let left =
        normalize_json_correlation_endpoint(relation.left, "relation left", context, services)?;
    let right =
        normalize_json_correlation_endpoint(relation.right, "relation right", context, services)?;
    if left.pointers.len() != right.pointers.len() {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} relation needs the same number of left and right JSON pointers"
        )));
    }
    if !matches!(
        relation.operator,
        ComposeJsonRelationOperator::Equals | ComposeJsonRelationOperator::NotEquals
    ) && left.pointers.len() != 1
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} numeric relation needs one left and one right JSON pointer"
        )));
    }
    if relation.order.is_some() && left.service != right.service {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} ordered relation needs the same left and right service"
        )));
    }
    Ok(SerialRelationPlan {
        left,
        right,
        operator: relation.operator,
        order: relation.order,
        quantifier: relation.quantifier,
        occurs: relation
            .occurs
            .map(|occurs| normalize_serial_match_count(occurs, context))
            .transpose()?,
    })
}

fn normalize_serial_path(
    path: ComposeSerialPath,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<SerialPathPlan, ComposeError> {
    if let Some(service) = &path.service {
        if !services.contains_key(service) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} path references unknown service {service:?}"
            )));
        }
    }
    if path.pointers.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} path needs at least one JSON pointer"
        )));
    }
    if path.steps.len() < 2 {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} path needs at least two JSON event steps"
        )));
    }
    let mut pointers = BTreeSet::new();
    for pointer in &path.pointers {
        if !valid_json_pointer(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} path has invalid JSON pointer {pointer:?}"
            )));
        }
        if !pointers.insert(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} path repeats JSON pointer {pointer:?}"
            )));
        }
    }
    Ok(SerialPathPlan {
        service: path.service,
        pointers: path.pointers,
        steps: path
            .steps
            .into_iter()
            .map(|step| normalize_json_predicate(step, context, false))
            .collect::<Result<_, _>>()?,
        quantifier: path.quantifier,
        occurs: path
            .occurs
            .map(|occurs| normalize_serial_match_count(occurs, context))
            .transpose()?,
    })
}

fn normalize_serial_workflow(
    workflow: ComposeSerialWorkflow,
    context: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<SerialWorkflowPlan, ComposeError> {
    if workflow.pointers.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} workflow needs at least one JSON pointer"
        )));
    }
    if workflow.stages.len() < 2 {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} workflow needs at least two service stages"
        )));
    }
    let mut pointers = BTreeSet::new();
    for pointer in &workflow.pointers {
        if !valid_json_pointer(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} workflow has invalid JSON pointer {pointer:?}"
            )));
        }
        if !pointers.insert(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} workflow repeats JSON pointer {pointer:?}"
            )));
        }
    }
    let mut stage_services = BTreeSet::new();
    let stages = workflow
        .stages
        .into_iter()
        .map(|stage| {
            if !services.contains_key(&stage.service) {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} workflow references unknown service {:?}",
                    stage.service
                )));
            }
            if !stage_services.insert(stage.service.clone()) {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} workflow repeats service {:?}",
                    stage.service
                )));
            }
            if stage.steps.is_empty() {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} workflow stage {:?} needs at least one JSON event step",
                    stage.service
                )));
            }
            let stage_pointers = if stage.pointers.is_empty() {
                workflow.pointers.clone()
            } else {
                stage.pointers
            };
            if stage_pointers.len() != workflow.pointers.len() {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} workflow stage {:?} needs {} key pointers",
                    stage.service,
                    workflow.pointers.len()
                )));
            }
            let mut unique_pointers = BTreeSet::new();
            for pointer in &stage_pointers {
                if !valid_json_pointer(pointer) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign {context} workflow stage {:?} has invalid JSON pointer {pointer:?}",
                        stage.service
                    )));
                }
                if !unique_pointers.insert(pointer) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign {context} workflow stage {:?} repeats JSON pointer {pointer:?}",
                        stage.service
                    )));
                }
            }
            Ok(SerialWorkflowStagePlan {
                service: stage.service,
                pointers: stage_pointers,
                steps: stage
                    .steps
                    .into_iter()
                    .map(|step| normalize_json_predicate(step, context, false))
                    .collect::<Result<_, _>>()?,
            })
        })
        .collect::<Result<Vec<_>, ComposeError>>()?;
    Ok(SerialWorkflowPlan {
        pointers: workflow.pointers,
        stages,
        quantifier: workflow.quantifier,
        occurs: workflow
            .occurs
            .map(|occurs| normalize_serial_match_count(occurs, context))
            .transpose()?,
    })
}

fn normalize_serial_match_count(
    occurs: ComposeSerialMatchCount,
    context: &str,
) -> Result<SerialMatchCountPlan, ComposeError> {
    let bounds = usize::from(occurs.exactly.is_some())
        + usize::from(occurs.at_least.is_some())
        + usize::from(occurs.at_most.is_some());
    if bounds == 0 || (occurs.exactly.is_some() && bounds != 1) {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} match count needs exactly or at_least and/or at_most"
        )));
    }
    if occurs.exactly == Some(0) || occurs.at_least == Some(0) || occurs.at_most == Some(0) {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} match count bounds must be at least one"
        )));
    }
    if occurs
        .at_least
        .zip(occurs.at_most)
        .is_some_and(|(at_least, at_most)| at_least > at_most)
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} match count at_least cannot exceed at_most"
        )));
    }
    Ok(SerialMatchCountPlan {
        exactly: occurs.exactly,
        at_least: occurs.at_least,
        at_most: occurs.at_most,
    })
}

fn normalize_serial_predicate(
    predicate: ComposeSerialPredicate,
    context: &str,
) -> Result<SerialPredicatePlan, ComposeError> {
    normalize_serial_predicate_with_captures(predicate, context, false)
}

fn normalize_serial_predicate_with_captures(
    predicate: ComposeSerialPredicate,
    context: &str,
    allow_captures: bool,
) -> Result<SerialPredicatePlan, ComposeError> {
    let has_sequence = !predicate.sequence.is_empty();
    let has_occurs = predicate.occurs.is_some();
    if has_sequence && has_occurs {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} cannot combine sequence with occurs"
        )));
    }
    if (has_sequence || has_occurs)
        && (predicate.contains.is_some()
            || predicate.matches.is_some()
            || predicate.json.is_some()
            || !predicate.all.is_empty()
            || !predicate.any.is_empty()
            || !predicate.none.is_empty())
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} cannot combine sequence with another nested predicate"
        )));
    }
    if predicate.contains.as_ref().is_some_and(String::is_empty) {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has an empty nested contains value"
        )));
    }
    if predicate.matches.as_ref().is_some_and(String::is_empty) {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has an empty nested matches value"
        )));
    }
    if let Some(expression) = &predicate.matches {
        Regex::new(expression).map_err(|error| {
            ComposeError::Invalid(format!(
                "campaign {context} has invalid nested regex {expression:?}: {error}"
            ))
        })?;
    }
    if predicate.contains.is_none()
        && predicate.matches.is_none()
        && predicate.json.is_none()
        && predicate.all.is_empty()
        && predicate.any.is_empty()
        && predicate.none.is_empty()
        && predicate.sequence.is_empty()
        && predicate.occurs.is_none()
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has an empty nested predicate"
        )));
    }
    let sequence = predicate
        .sequence
        .into_iter()
        .map(|child| normalize_sequence_item(child, context))
        .collect::<Result<Vec<_>, _>>()?;
    validate_sequence_captures(&sequence, context)?;
    Ok(SerialPredicatePlan {
        contains: predicate.contains,
        matches: predicate.matches,
        json: predicate
            .json
            .map(|json| normalize_json_predicate(json, context, allow_captures))
            .transpose()?,
        all: predicate
            .all
            .into_iter()
            .map(|child| normalize_serial_predicate(child, context))
            .collect::<Result<_, _>>()?,
        any: predicate
            .any
            .into_iter()
            .map(|child| normalize_serial_predicate(child, context))
            .collect::<Result<_, _>>()?,
        none: predicate
            .none
            .into_iter()
            .map(|child| normalize_serial_predicate(child, context))
            .collect::<Result<_, _>>()?,
        sequence,
        occurs: predicate
            .occurs
            .map(|occurs| normalize_serial_occurrence(occurs, context))
            .transpose()?,
    })
}

fn normalize_json_predicate(
    json: ComposeJsonPredicate,
    context: &str,
    allow_captures: bool,
) -> Result<JsonPredicatePlan, ComposeError> {
    if json.query.is_none()
        && json.fields.is_empty()
        && json.where_.is_empty()
        && json.arrays.is_empty()
        && json.all.is_empty()
        && json.any.is_empty()
        && json.none.is_empty()
        && json.capture.is_empty()
        && json.equals_capture.is_empty()
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has an empty nested JSON predicate"
        )));
    }
    if let Some(query) = &json.query {
        if query.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} has an empty JSONPath query"
            )));
        }
        JsonPath::parse(query).map_err(|error| {
            ComposeError::Invalid(format!(
                "campaign {context} has invalid JSONPath query {query:?}: {error}"
            ))
        })?;
    }
    for pointer in json.fields.keys() {
        if !valid_json_pointer(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} has invalid nested JSON pointer {pointer:?}"
            )));
        }
    }
    for condition in &json.where_ {
        validate_json_condition(condition, context)?;
    }
    if (!json.capture.is_empty() || !json.equals_capture.is_empty()) && !allow_captures {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} JSON captures are only allowed in sequence items"
        )));
    }
    for (name, pointer) in &json.capture {
        validate_name("JSON capture", name)?;
        if !valid_json_pointer(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} has invalid JSON capture pointer {pointer:?}"
            )));
        }
    }
    for (pointer, name) in &json.equals_capture {
        if !valid_json_pointer(pointer) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} has invalid JSON capture comparison pointer {pointer:?}"
            )));
        }
        validate_name("JSON capture", name)?;
    }
    let arrays = json
        .arrays
        .into_iter()
        .map(|array| normalize_json_array_predicate(array, context))
        .collect::<Result<_, _>>()?;
    let all = json
        .all
        .into_iter()
        .map(|predicate| normalize_json_predicate(predicate, context, false))
        .collect::<Result<_, _>>()?;
    let any = json
        .any
        .into_iter()
        .map(|predicate| normalize_json_predicate(predicate, context, false))
        .collect::<Result<_, _>>()?;
    let none = json
        .none
        .into_iter()
        .map(|predicate| normalize_json_predicate(predicate, context, false))
        .collect::<Result<_, _>>()?;
    Ok(JsonPredicatePlan {
        query: json.query,
        fields: json.fields,
        where_: json
            .where_
            .into_iter()
            .map(|condition| JsonConditionPlan {
                pointer: condition.pointer,
                equals: condition.equals,
                matches: condition.matches,
                greater_than: condition.greater_than,
                greater_than_or_equal: condition.greater_than_or_equal,
                less_than: condition.less_than,
                less_than_or_equal: condition.less_than_or_equal,
                exists: condition.exists,
            })
            .collect(),
        arrays,
        all,
        any,
        none,
        capture: json.capture,
        equals_capture: json.equals_capture,
    })
}

fn normalize_json_array_predicate(
    array: ComposeJsonArrayPredicate,
    context: &str,
) -> Result<JsonArrayPredicatePlan, ComposeError> {
    if !valid_json_pointer(&array.pointer) {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has invalid nested JSON array pointer {:?}",
            array.pointer
        )));
    }
    let modes = usize::from(array.any.is_some())
        + usize::from(array.all.is_some())
        + usize::from(array.none.is_some());
    if modes != 1 {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} JSON array {:?} needs exactly one of any, all, or none",
            array.pointer
        )));
    }
    Ok(JsonArrayPredicatePlan {
        pointer: array.pointer,
        any: array
            .any
            .map(|predicate| normalize_json_predicate(*predicate, context, false).map(Box::new))
            .transpose()?,
        all: array
            .all
            .map(|predicate| normalize_json_predicate(*predicate, context, false).map(Box::new))
            .transpose()?,
        none: array
            .none
            .map(|predicate| normalize_json_predicate(*predicate, context, false).map(Box::new))
            .transpose()?,
    })
}

fn normalize_serial_occurrence(
    occurs: ComposeSerialOccurrence,
    context: &str,
) -> Result<SerialOccurrencePlan, ComposeError> {
    let bounds = usize::from(occurs.exactly.is_some())
        + usize::from(occurs.at_least.is_some())
        + usize::from(occurs.at_most.is_some());
    if bounds == 0 || (occurs.exactly.is_some() && bounds != 1) {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} occurs needs exactly or at_least and/or at_most"
        )));
    }
    if occurs.exactly == Some(0) || occurs.at_least == Some(0) || occurs.at_most == Some(0) {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} occurs bounds must be at least one"
        )));
    }
    if occurs
        .at_least
        .zip(occurs.at_most)
        .is_some_and(|(at_least, at_most)| at_least > at_most)
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} occurs at_least cannot exceed at_most"
        )));
    }
    let predicate = normalize_serial_predicate(*occurs.predicate, context)?;
    let leaves = usize::from(predicate.contains.is_some())
        + usize::from(predicate.matches.is_some())
        + usize::from(predicate.json.is_some());
    if leaves != 1
        || !predicate.all.is_empty()
        || !predicate.any.is_empty()
        || !predicate.none.is_empty()
        || !predicate.sequence.is_empty()
        || predicate.occurs.is_some()
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} occurs predicate must contain exactly one of contains, matches, or json"
        )));
    }
    Ok(SerialOccurrencePlan {
        predicate: Box::new(predicate),
        exactly: occurs.exactly,
        at_least: occurs.at_least,
        at_most: occurs.at_most,
    })
}

fn normalize_sequence_item(
    predicate: ComposeSerialPredicate,
    context: &str,
) -> Result<SerialPredicatePlan, ComposeError> {
    let leaves = usize::from(predicate.contains.is_some())
        + usize::from(predicate.matches.is_some())
        + usize::from(predicate.json.is_some());
    if leaves != 1
        || !predicate.all.is_empty()
        || !predicate.any.is_empty()
        || !predicate.none.is_empty()
        || !predicate.sequence.is_empty()
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} sequence items must contain exactly one of contains, matches, or json"
        )));
    }
    normalize_serial_predicate_with_captures(predicate, context, true)
}

fn validate_sequence_captures(
    sequence: &[SerialPredicatePlan],
    context: &str,
) -> Result<(), ComposeError> {
    let mut captures = BTreeSet::new();
    for predicate in sequence {
        let Some(json) = &predicate.json else {
            continue;
        };
        for name in json.equals_capture.values() {
            if !captures.contains(name) {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} JSON capture {name:?} must be declared by an earlier sequence item"
                )));
            }
        }
        for name in json.capture.keys() {
            if !captures.insert(name) {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} JSON capture {name:?} is declared more than once in a sequence"
                )));
            }
        }
    }
    Ok(())
}

fn validate_json_condition(
    condition: &ComposeJsonCondition,
    context: &str,
) -> Result<(), ComposeError> {
    if !valid_json_pointer(&condition.pointer) {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} has invalid nested JSON pointer {:?}",
            condition.pointer
        )));
    }
    let operators = usize::from(condition.equals.is_some())
        + usize::from(condition.matches.is_some())
        + usize::from(condition.greater_than.is_some())
        + usize::from(condition.greater_than_or_equal.is_some())
        + usize::from(condition.less_than.is_some())
        + usize::from(condition.less_than_or_equal.is_some())
        + usize::from(condition.exists.is_some());
    if operators != 1 {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} JSON condition {:?} needs exactly one operator",
            condition.pointer
        )));
    }
    if let Some(expression) = &condition.matches {
        if expression.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} has an empty nested JSON regex"
            )));
        }
        Regex::new(expression).map_err(|error| {
            ComposeError::Invalid(format!(
                "campaign {context} has invalid nested JSON regex {expression:?}: {error}"
            ))
        })?;
    }
    for value in [
        condition.greater_than,
        condition.greater_than_or_equal,
        condition.less_than,
        condition.less_than_or_equal,
    ] {
        if value.is_some_and(|value| !value.is_finite()) {
            return Err(ComposeError::Invalid(format!(
                "campaign {context} has a non-finite nested JSON number"
            )));
        }
    }
    Ok(())
}

fn valid_json_pointer(pointer: &str) -> bool {
    let Some(rest) = pointer.strip_prefix('/') else {
        return false;
    };
    let bytes = rest.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            if !matches!(bytes.get(index + 1), Some(b'0' | b'1')) {
                return false;
            }
            index += 1;
        }
        index += 1;
    }
    true
}

fn validate_campaign_operation_rules(
    operations: &[OperationPlan],
    stages: &[String],
    initial_state: &BTreeMap<String, String>,
) -> Result<(), ComposeError> {
    for (name, value) in initial_state {
        validate_name("campaign state", name)?;
        validate_name("campaign state value", value)?;
    }
    let mut stage_names = BTreeSet::new();
    for stage in stages {
        validate_name("campaign stage", stage)?;
        if !stage_names.insert(stage) {
            return Err(ComposeError::Invalid(format!(
                "campaign stage {stage:?} is declared more than once"
            )));
        }
    }
    let names = operations
        .iter()
        .map(|operation| operation.name.as_str())
        .collect::<BTreeSet<_>>();
    let input_names = operations
        .iter()
        .map(|operation| {
            (
                operation.name.as_str(),
                operation
                    .inputs
                    .iter()
                    .map(|input| input.name.as_str())
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for operation in operations {
        if let Some(stage) = &operation.stage {
            if !stage_names.contains(stage) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} references unknown stage {stage:?}",
                    operation.name
                )));
            }
        } else if !stages.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} must declare one of the campaign stages",
                operation.name
            )));
        }
        let mut requirements = BTreeSet::new();
        for requirement in &operation.requires {
            validate_name("campaign operation requirement", requirement)?;
            if !names.contains(requirement.as_str()) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} requires unknown operation {:?}",
                    operation.name, requirement
                )));
            }
            if !requirements.insert(requirement) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} requires {:?} more than once",
                    operation.name, requirement
                )));
            }
        }
        let mut exclusions = BTreeSet::new();
        for exclusion in &operation.excludes {
            validate_name("campaign operation exclusion", exclusion)?;
            if !names.contains(exclusion.as_str()) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} excludes unknown operation {:?}",
                    operation.name, exclusion
                )));
            }
            if !exclusions.insert(exclusion) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} excludes {:?} more than once",
                    operation.name, exclusion
                )));
            }
            if requirements.contains(exclusion) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} both requires and excludes {:?}",
                    operation.name, exclusion
                )));
            }
        }
        let mut required_markers = BTreeSet::new();
        for marker in &operation.requires_markers {
            validate_name("campaign operation required marker", marker)?;
            if !required_markers.insert(marker) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} requires marker {:?} more than once",
                    operation.name, marker
                )));
            }
        }
        let mut excluded_markers = BTreeSet::new();
        for marker in &operation.excludes_markers {
            validate_name("campaign operation excluded marker", marker)?;
            if !excluded_markers.insert(marker) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} excludes marker {:?} more than once",
                    operation.name, marker
                )));
            }
            if required_markers.contains(marker) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} both requires and excludes marker {:?}",
                    operation.name, marker
                )));
            }
        }
        if operation
            .max_uses
            .is_some_and(|maximum| maximum == 0 || maximum > 4)
        {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} max_uses must be between 1 and 4",
                operation.name
            )));
        }
        validate_campaign_state_rule(
            &operation.requires_state,
            "requires_state",
            &format!("campaign operation {:?}", operation.name),
            initial_state,
        )?;
        validate_campaign_state_rule(
            &operation.sets_state,
            "sets_state",
            &format!("campaign operation {:?}", operation.name),
            initial_state,
        )?;
        for input in &operation.inputs {
            let context = format!(
                "campaign operation {:?} input {:?}",
                operation.name, input.name
            );
            let mut requirements = BTreeSet::new();
            for reference in &input.requires {
                validate_operation_input_reference(reference, &context, "requires", &input_names)?;
                let key = operation_input_reference_name(reference);
                if !requirements.insert(key.clone()) {
                    return Err(ComposeError::Invalid(format!(
                        "{context} requires {key:?} more than once"
                    )));
                }
            }
            let mut exclusions = BTreeSet::new();
            for reference in &input.excludes {
                validate_operation_input_reference(reference, &context, "excludes", &input_names)?;
                let key = operation_input_reference_name(reference);
                if !exclusions.insert(key.clone()) {
                    return Err(ComposeError::Invalid(format!(
                        "{context} excludes {key:?} more than once"
                    )));
                }
                if requirements.contains(&key) {
                    return Err(ComposeError::Invalid(format!(
                        "{context} both requires and excludes {key:?}"
                    )));
                }
            }
            if input
                .max_uses
                .is_some_and(|maximum| maximum == 0 || maximum > 4)
            {
                return Err(ComposeError::Invalid(format!(
                    "{context} max_uses must be between 1 and 4"
                )));
            }
            validate_campaign_state_rule(
                &input.requires_state,
                "requires_state",
                &context,
                initial_state,
            )?;
            validate_campaign_state_rule(&input.sets_state, "sets_state", &context, initial_state)?;
        }
    }
    let mut reachable = BTreeSet::new();
    loop {
        let before = reachable.len();
        for operation in operations {
            if operation
                .requires
                .iter()
                .all(|requirement| reachable.contains(requirement.as_str()))
            {
                reachable.insert(operation.name.as_str());
            }
        }
        if reachable.len() == before {
            break;
        }
    }
    if reachable.len() == operations.len() {
        let mut reachable_inputs = BTreeSet::new();
        loop {
            let before = reachable_inputs.len();
            for operation in operations {
                if !operation.requires.iter().all(|requirement| {
                    reachable_inputs
                        .iter()
                        .any(|(prior_operation, _)| prior_operation == requirement)
                }) {
                    continue;
                }
                for input in &operation.inputs {
                    if input.requires.iter().all(|reference| {
                        reachable_inputs
                            .iter()
                            .any(|(prior_operation, prior_input)| {
                                prior_operation == &reference.operation
                                    && reference
                                        .input
                                        .as_ref()
                                        .is_none_or(|expected| prior_input == expected)
                            })
                    }) {
                        reachable_inputs.insert((operation.name.as_str(), input.name.as_str()));
                    }
                }
            }
            if reachable_inputs.len() == before {
                break;
            }
        }
        let blocked = operations
            .iter()
            .flat_map(|operation| {
                operation.inputs.iter().filter_map(|input| {
                    (!reachable_inputs.contains(&(operation.name.as_str(), input.name.as_str())))
                        .then(|| format!("{}[{}]", operation.name, input.name))
                })
            })
            .collect::<Vec<_>>();
        if blocked.is_empty() {
            return Ok(());
        }
        return Err(ComposeError::Invalid(format!(
            "campaign operation input requirements cannot reach: {}",
            blocked.join(", ")
        )));
    }
    let blocked = operations
        .iter()
        .filter(|operation| !reachable.contains(operation.name.as_str()))
        .map(|operation| operation.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(ComposeError::Invalid(format!(
        "campaign operation requirements cannot reach: {blocked}"
    )))
}

fn validate_test_command_model(
    operations: &[OperationPlan],
    stages: &[String],
) -> Result<(), ComposeError> {
    let declared = operations
        .iter()
        .filter(|operation| operation.command.is_some())
        .count();
    if declared == 0 {
        return Ok(());
    }
    if declared != operations.len() {
        return Err(ComposeError::Invalid(
            "test-command campaigns must assign command to every operation".to_owned(),
        ));
    }
    if !stages.is_empty() {
        return Err(ComposeError::Invalid(
            "test-command campaigns use command lifecycle roles instead of stages".to_owned(),
        ));
    }
    let templates = operations
        .iter()
        .map(|operation| operation.test_template.as_deref())
        .collect::<BTreeSet<_>>();
    for template in templates {
        if !operations.iter().any(|operation| {
            operation.test_template.as_deref() == template
                && matches!(
                    operation.command,
                    Some(
                        ComposeTestCommand::ParallelDriver
                            | ComposeTestCommand::SerialDriver
                            | ComposeTestCommand::SingletonDriver
                            | ComposeTestCommand::Anytime
                    )
                )
        }) {
            let scope = template
                .map(|template| format!("test template {template:?}"))
                .unwrap_or_else(|| "test-command campaign".to_owned());
            return Err(ComposeError::Invalid(format!(
                "{scope} needs a parallel_driver, serial_driver, singleton_driver, or anytime command"
            )));
        }
    }
    for operation in operations {
        let command = operation.command.expect("all command roles were declared");
        let asynchronous = matches!(
            operation.shell_phase,
            Some(ComposeShellPhase::Launch | ComposeShellPhase::Completion)
        );
        if asynchronous
            && !matches!(
                command,
                ComposeTestCommand::ParallelDriver | ComposeTestCommand::Anytime
            )
        {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} uses an asynchronous shell phase outside parallel_driver or anytime",
                operation.name
            )));
        }
    }
    Ok(())
}

fn operation_input_reference_name(reference: &OperationInputReferencePlan) -> String {
    reference
        .input
        .as_ref()
        .map(|input| format!("{}[{input}]", reference.operation))
        .unwrap_or_else(|| reference.operation.clone())
}

fn validate_operation_input_reference(
    reference: &OperationInputReferencePlan,
    context: &str,
    rule: &str,
    input_names: &BTreeMap<&str, BTreeSet<&str>>,
) -> Result<(), ComposeError> {
    let Some(inputs) = input_names.get(reference.operation.as_str()) else {
        return Err(ComposeError::Invalid(format!(
            "{context} {rule} unknown operation {:?}",
            reference.operation
        )));
    };
    if let Some(input) = &reference.input {
        if !inputs.contains(input.as_str()) {
            return Err(ComposeError::Invalid(format!(
                "{context} {rule} unknown input {:?}",
                operation_input_reference_name(reference)
            )));
        }
    }
    Ok(())
}

fn validate_campaign_state_rule(
    rule: &BTreeMap<String, String>,
    name: &str,
    context: &str,
    initial_state: &BTreeMap<String, String>,
) -> Result<(), ComposeError> {
    for (key, value) in rule {
        validate_name("campaign state", key)?;
        validate_name("campaign state value", value)?;
        if !initial_state.contains_key(key) {
            return Err(ComposeError::Invalid(format!(
                "{context} {name} references undeclared state {key:?}"
            )));
        }
    }
    Ok(())
}

/// Normalize one fault window's `until` barrier. Only fault kinds with an
/// automatic recovery can close a window: recovery kinds, lifecycle faults,
/// and custom faults have no inverse to apply at the barrier.
fn normalize_campaign_fault_window(
    candidate: &ComposeCampaignFault,
    operations: &[OperationPlan],
) -> Result<Option<String>, ComposeError> {
    let Some(until) = candidate.until.as_deref() else {
        return Ok(None);
    };
    let recoverable = matches!(
        candidate.kind,
        CampaignFaultKind::Partition
            | CampaignFaultKind::LinkPartition
            | CampaignFaultKind::LinkFault
            | CampaignFaultKind::LinkClog
            | CampaignFaultKind::NetworkFault
            | CampaignFaultKind::PacketFault
            | CampaignFaultKind::CpuThrottle
            | CampaignFaultKind::ClockRate
            | CampaignFaultKind::ServiceStop
            | CampaignFaultKind::ServiceKill
            | CampaignFaultKind::StorageFault
    );
    if !recoverable {
        return Err(ComposeError::Invalid(format!(
            "campaign {:?} faults have no automatic recovery and accept no until window",
            candidate.kind
        )));
    }
    let (until, until_input) = normalize_campaign_fault_after(until, operations)?;
    if until_input.is_some() {
        return Err(ComposeError::Invalid(
            "campaign fault windows accept only an operation name as until".to_owned(),
        ));
    }
    if candidate.after.is_some() && candidate.after == candidate.until {
        return Err(ComposeError::Invalid(
            "campaign fault window must close at an operation after its trigger".to_owned(),
        ));
    }
    Ok(until)
}

fn normalize_campaign_fault_after(
    after: &str,
    operations: &[OperationPlan],
) -> Result<(Option<String>, Option<OperationInputReferencePlan>), ComposeError> {
    let reference = normalize_operation_input_references(vec![after.to_owned()], "after", "fault")?
        .pop()
        .expect("one campaign fault barrier is normalized");
    let Some(operation) = operations
        .iter()
        .find(|operation| operation.name == reference.operation)
    else {
        return Err(ComposeError::Invalid(format!(
            "campaign action after references unknown operation {after:?}",
        )));
    };
    if matches!(
        operation.command,
        Some(
            ComposeTestCommand::First
                | ComposeTestCommand::Eventually
                | ComposeTestCommand::Finally
        )
    ) {
        return Err(ComposeError::Invalid(format!(
            "campaign action after cannot target lifecycle-protected test command {:?}",
            operation.name
        )));
    }
    if let Some(input) = &reference.input {
        if !operation
            .inputs
            .iter()
            .any(|candidate| candidate.name == *input)
        {
            return Err(ComposeError::Invalid(format!(
                "campaign action after references unknown input {:?}",
                operation_input_reference_name(&reference)
            )));
        }
        return Ok((None, Some(reference)));
    }
    Ok((Some(reference.operation), None))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_faults(
    service: &str,
    faults: Vec<ComposeFault>,
    has_virtual_time: bool,
) -> Result<Vec<FaultPlan>, ComposeError> {
    let mut previous_round = 0;
    let mut paused_until = 0;
    let mut plans = Vec::with_capacity(faults.len());
    for fault in faults {
        if fault.at_round == 0 {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} fault at_round must be greater than zero"
            )));
        }
        if fault.at_round <= previous_round {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} faults must use strictly increasing at_round values"
            )));
        }
        if fault.at_round < paused_until {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} fault at round {} falls within a pause ending at round {paused_until}",
                fault.at_round
            )));
        }
        let plan = match fault.kind {
            FaultKind::Pause => {
                let duration = fault.duration_rounds.ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "service {service:?} pause fault requires duration_rounds"
                    ))
                })?;
                if duration == 0 || fault.nanoseconds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} pause fault requires a positive duration_rounds and no nanoseconds"
                    )));
                }
                paused_until = fault.at_round.checked_add(duration).ok_or_else(|| {
                    ComposeError::Invalid(format!("service {service:?} pause duration overflows"))
                })?;
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::Pause,
                    duration_rounds: Some(duration),
                    nanoseconds: None,
                }
            }
            FaultKind::Restart => {
                if fault.duration_rounds.is_some() || fault.nanoseconds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} restart fault takes no duration_rounds or nanoseconds"
                    )));
                }
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::Restart,
                    duration_rounds: None,
                    nanoseconds: None,
                }
            }
            FaultKind::ClockJump => {
                let nanoseconds = fault.nanoseconds.ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "service {service:?} clock_jump fault requires nanoseconds"
                    ))
                })?;
                if !has_virtual_time || nanoseconds == 0 || fault.duration_rounds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} clock_jump requires virtual_time, non-zero nanoseconds, and no duration_rounds"
                    )));
                }
                if nanoseconds.unsigned_abs() > 3_600_000_000_000 {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} clock_jump nanoseconds magnitude must be at most 3600000000000"
                    )));
                }
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::ClockJump,
                    duration_rounds: None,
                    nanoseconds: Some(nanoseconds),
                }
            }
        };
        previous_round = plan.at_round;
        plans.push(plan);
    }
    Ok(plans)
}

fn validate_name(kind: &str, value: &str) -> Result<(), ComposeError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ComposeError::Invalid(format!(
            "{kind} name {value:?} must use only letters, digits, '-' and '_'"
        )));
    }
    Ok(())
}

fn normalize_operation_input_references(
    references: Vec<String>,
    rule: &str,
    operation: &str,
) -> Result<Vec<OperationInputReferencePlan>, ComposeError> {
    references
        .into_iter()
        .map(|reference| {
            let parsed = if let Some((name, input)) = reference.split_once('[') {
                let Some(input) = input.strip_suffix(']') else {
                    return Err(ComposeError::Invalid(format!(
                        "campaign operation {operation:?} input {rule} reference {reference:?} must end with ']'")
                    ));
                };
                if input.contains('[') || input.contains(']') {
                    return Err(ComposeError::Invalid(format!(
                        "campaign operation {operation:?} input {rule} reference {reference:?} is malformed"
                    )));
                }
                OperationInputReferencePlan {
                    operation: name.to_owned(),
                    input: Some(input.to_owned()),
                }
            } else {
                OperationInputReferencePlan {
                    operation: reference.clone(),
                    input: None,
                }
            };
            validate_name("campaign operation input reference", &parsed.operation)?;
            if let Some(input) = &parsed.input {
                validate_name("campaign operation input reference", input)?;
            }
            Ok(parsed)
        })
        .collect()
}

struct NormalizedOperationInputGrammar {
    source: OperationInputGrammarPlan,
    inputs: Vec<OperationInputPlan>,
}

/// Expand a small, finite input grammar during Compose normalization. The
/// persisted plan contains both the source grammar (for inspection) and the
/// concrete leaves (for exact replay).
fn normalize_operation_input_grammar(
    grammar: &ComposeOperationInputGrammar,
    operation: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<NormalizedOperationInputGrammar, ComposeError> {
    let variables = operation_input_template_variables(&grammar.template, operation, "template")?;
    if variables.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign operation {operation:?} input_grammar template needs at least one {{variable}}"
        )));
    }
    let mut unique_variables = Vec::new();
    for variable in variables {
        if !unique_variables.contains(&variable) {
            unique_variables.push(variable);
        }
    }
    let captures =
        normalize_operation_input_captures(grammar.input_captures.clone(), operation, services)?;
    let capture_names = captures.keys().cloned().collect::<BTreeSet<_>>();
    if !capture_names.is_subset(&unique_variables.iter().cloned().collect()) {
        return Err(ComposeError::Invalid(format!(
            "campaign operation {operation:?} input_grammar input_captures must name template placeholders"
        )));
    }
    let choice_variables = unique_variables
        .iter()
        .filter(|variable| !capture_names.contains(*variable))
        .cloned()
        .collect::<Vec<_>>();
    if choice_variables.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign operation {operation:?} input_grammar needs at least one choice placeholder"
        )));
    }
    for variable in &choice_variables {
        let Some(variants) = grammar.choices.get(variable) else {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar has no choices for {variable:?}"
            )));
        };
        if variants.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar choices for {variable:?} must not be empty"
            )));
        }
        for name in variants.keys() {
            validate_name("campaign operation grammar choice", name)?;
        }
    }
    for variable in grammar.choices.keys() {
        validate_name("campaign operation grammar variable", variable)?;
        if !choice_variables.contains(variable) {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar declares unused choices for {variable:?}"
            )));
        }
    }

    let name_template = grammar.name_template.clone().unwrap_or_else(|| {
        choice_variables
            .iter()
            .map(|variable| format!("{variable}-{{{variable}}}"))
            .collect::<Vec<_>>()
            .join("--")
    });
    let name_variables =
        operation_input_template_variables(&name_template, operation, "name_template")?;
    if name_variables
        .iter()
        .any(|variable| !choice_variables.contains(variable))
    {
        return Err(ComposeError::Invalid(format!(
            "campaign operation {operation:?} input_grammar name_template may reference only choices"
        )));
    }
    let mut combinations = vec![BTreeMap::<String, (String, String)>::new()];
    for variable in &choice_variables {
        let variants = &grammar.choices[variable];
        let mut next = Vec::with_capacity(combinations.len() * variants.len());
        for bindings in combinations {
            for (choice, value) in variants {
                let mut binding = bindings.clone();
                binding.insert(variable.clone(), (choice.clone(), value.clone()));
                next.push(binding);
            }
        }
        if next.len() > 64 {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar expands to more than 64 cases"
            )));
        }
        combinations = next;
    }

    let mut generated = BTreeSet::new();
    let mut inputs = Vec::with_capacity(combinations.len());
    for mut bindings in combinations {
        for name in &capture_names {
            bindings.insert(name.clone(), (name.clone(), format!("{{{name}}}")));
        }
        let input = render_operation_input_template(
            &grammar.template,
            &bindings,
            false,
            operation,
            "template",
        )?;
        if input.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar generated an empty input"
            )));
        }
        let name = render_operation_input_template(
            &name_template,
            &bindings,
            true,
            operation,
            "name_template",
        )?;
        validate_name("campaign operation input", &name)?;
        if !generated.insert(name.clone()) {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar generates duplicate case {name:?}"
            )));
        }
        let rules = grammar.cases.get(&name).cloned().unwrap_or_default();
        inputs.push(OperationInputPlan {
            name,
            input_hex: captures
                .is_empty()
                .then(|| hex(input.as_bytes()))
                .unwrap_or_default(),
            choices: BTreeMap::new(),
            thread_schedule: Vec::new(),
            input_template: (!captures.is_empty()).then_some(input),
            input_captures: captures.clone(),
            requires: normalize_operation_input_references(rules.requires, "requires", operation)?,
            excludes: normalize_operation_input_references(rules.excludes, "excludes", operation)?,
            max_uses: rules.max_uses,
            requires_state: rules.requires_state,
            sets_state: rules.sets_state,
        });
    }
    for name in grammar.cases.keys() {
        validate_name("campaign operation grammar case", name)?;
        if !generated.contains(name) {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar has rules for unknown case {name:?}"
            )));
        }
    }
    Ok(NormalizedOperationInputGrammar {
        source: OperationInputGrammarPlan {
            template: grammar.template.clone(),
            name_template,
            choices: grammar.choices.clone(),
            input_captures: captures,
        },
        inputs,
    })
}

fn normalize_operation_input_template(
    template: String,
    captures: BTreeMap<String, ComposeOperationInputCapture>,
    operation: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<(String, BTreeMap<String, OperationInputCapturePlan>), ComposeError> {
    if template.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "campaign operation {operation:?} input_template must not be empty"
        )));
    }
    let variables = operation_input_template_variables(&template, operation, "input_template")?;
    let variables = variables.into_iter().collect::<BTreeSet<_>>();
    if variables != captures.keys().cloned().collect() {
        return Err(ComposeError::Invalid(format!(
            "campaign operation {operation:?} input_template placeholders must exactly match input_captures"
        )));
    }
    let captures = normalize_operation_input_captures(captures, operation, services)?;
    Ok((template, captures))
}

fn normalize_operation_input_captures(
    captures: BTreeMap<String, ComposeOperationInputCapture>,
    operation: &str,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<BTreeMap<String, OperationInputCapturePlan>, ComposeError> {
    captures
        .into_iter()
        .map(|(name, capture)| {
            validate_name("campaign operation input capture", &name)?;
            if !valid_json_pointer(&capture.pointer) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {operation:?} input capture {name:?} has invalid JSON pointer {:?}",
                    capture.pointer
                )));
            }
            if let Some(service) = &capture.service {
                if !services.contains_key(service) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign operation {operation:?} input capture {name:?} names unknown service {service:?}"
                    )));
                }
            }
            let context = format!("operation {operation:?} input capture {name:?}");
            let forms = usize::from(capture.json.is_some())
                + usize::from(!capture.sequence.is_empty())
                + usize::from(capture.workflow.is_some());
            if forms != 1 {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} needs exactly one of json, sequence, or workflow"
                )));
            }
            if capture.workflow.is_some() && capture.service.is_some() {
                return Err(ComposeError::Invalid(format!(
                    "campaign {context} workflow selects its services in workflow stages, not service"
                )));
            }
            let json = capture
                .json
                .map(|json| normalize_json_predicate(json, &context, false))
                .transpose()?;
            let sequence = normalize_operation_input_capture_sequence(capture.sequence, &context)?;
            let workflow = capture
                .workflow
                .map(|workflow| normalize_serial_workflow(workflow, &context, services))
                .transpose()?;
            Ok((
                name,
                OperationInputCapturePlan {
                    service: capture.service,
                    pointer: capture.pointer,
                    json,
                    sequence,
                    workflow,
                    encoding: capture.encoding,
                    select: capture.select,
                },
            ))
        })
        .collect()
}

fn normalize_operation_input_capture_sequence(
    sequence: Vec<ComposeSerialPredicate>,
    context: &str,
) -> Result<Vec<SerialPredicatePlan>, ComposeError> {
    if sequence.is_empty() {
        return Ok(Vec::new());
    }
    let sequence = sequence
        .into_iter()
        .map(|predicate| normalize_sequence_item(predicate, context))
        .collect::<Result<Vec<_>, _>>()?;
    validate_sequence_captures(&sequence, context)?;
    if sequence
        .last()
        .is_none_or(|predicate| predicate.json.is_none())
    {
        return Err(ComposeError::Invalid(format!(
            "campaign {context} input capture sequence must end with a JSON event"
        )));
    }
    Ok(sequence)
}

fn operation_input_template_variables(
    template: &str,
    operation: &str,
    field: &str,
) -> Result<Vec<String>, ComposeError> {
    let mut variables = Vec::new();
    let mut cursor = 0;
    loop {
        let rest = &template[cursor..];
        let opening = rest.find('{');
        let closing = rest.find('}');
        let Some(relative_start) = opening else {
            if closing.is_some() {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {operation:?} input_grammar {field} has an unmatched '}}'"
                )));
            }
            break;
        };
        if closing.is_some_and(|close| close < relative_start) {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar {field} has an unmatched '}}'"
            )));
        }
        let start = cursor + relative_start;
        let value_start = start + 1;
        let Some(relative_end) = template[value_start..].find('}') else {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar {field} has an unclosed '{{'"
            )));
        };
        let end = value_start + relative_end;
        let variable = &template[value_start..end];
        if variable.contains('{') || variable.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar {field} has an invalid placeholder"
            )));
        }
        validate_name("campaign operation grammar variable", variable)?;
        variables.push(variable.to_owned());
        cursor = end + 1;
    }
    Ok(variables)
}

fn render_operation_input_template(
    template: &str,
    bindings: &BTreeMap<String, (String, String)>,
    names: bool,
    operation: &str,
    field: &str,
) -> Result<String, ComposeError> {
    let mut output = String::new();
    let mut cursor = 0;
    while let Some(relative_start) = template[cursor..].find('{') {
        let start = cursor + relative_start;
        output.push_str(&template[cursor..start]);
        let value_start = start + 1;
        let end = value_start
            + template[value_start..]
                .find('}')
                .expect("template placeholders were validated");
        let variable = &template[value_start..end];
        let Some((choice, value)) = bindings.get(variable) else {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {operation:?} input_grammar {field} references unknown variable {variable:?}"
            )));
        };
        output.push_str(if names { choice } else { value });
        cursor = end + 1;
    }
    output.push_str(&template[cursor..]);
    Ok(output)
}

/// Execute a locked plan with the Linux-only runner shipped beside `theseus`
/// in a published runtime bundle. Planning remains portable because the CLI
/// itself never links the Linux/KVM VMM.
pub fn test_compose(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let mut plan = load_compose_plan(path)?;
    plan.topology_runner = Some(installed_runner_artifact()?);
    let output = output.as_ref().to_path_buf();
    let plan_file = write_temporary_topology_plan(&plan, &output)?;
    let result = execute_topology(&plan_file, &output);
    let _ = fs::remove_file(&plan_file);
    result.map(|()| output)
}

/// Execute the topology's declared autonomous campaign.  The command uses the
/// same locked-artifact executor as `compose test`; the runner selects
/// campaign mode from the normalized plan instead of accepting host commands.
pub fn explore_compose(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    explore_compose_with(path, output, None, None, None, None)
}

/// Execute the topology's declared campaign with explicit fixed-budget and
/// guidance overrides. Comparing the same Compose file across guidance modes
/// at one budget is the fixed-budget search comparison the roadmap requires.
pub fn explore_compose_with(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
    max_runs: Option<u16>,
    guidance: Option<CampaignGuidance>,
    notify: Option<&str>,
    shard: Option<(u16, u16)>,
) -> Result<PathBuf, ComposeError> {
    let mut plan = load_compose_plan(&path)?;
    if plan.campaign.is_none() {
        return Err(ComposeError::Invalid(
            "Compose file has no x-theseus.campaign section".to_owned(),
        ));
    }
    let shard = shard.map(validate_campaign_shard).transpose()?;
    if let Some(campaign) = plan.campaign.as_mut() {
        if let Some(max_runs) = max_runs {
            campaign.max_runs = max_runs;
        }
        if let Some(guidance) = guidance {
            campaign.guidance = guidance;
        }
        campaign.shard = shard;
    }
    plan.topology_runner = Some(installed_runner_artifact()?);
    let output = output.as_ref().to_path_buf();
    let plan_file = write_temporary_topology_plan(&plan, &output)?;
    let result = execute_topology(&plan_file, &output);
    let _ = fs::remove_file(&plan_file);
    notify_campaign_completion(notify, &output);
    result.map(|()| output)
}

/// The completion evidence a notification hook observes: the retained
/// campaign status and the names of every property retained as failed.
fn campaign_completion_status(output: &Path) -> (String, Vec<String>) {
    let Ok(result) = fs::read(output.join("campaign-result.json")) else {
        return ("unknown".to_owned(), Vec::new());
    };
    let Ok(result) = serde_json::from_slice::<serde_json::Value>(&result) else {
        return ("unknown".to_owned(), Vec::new());
    };
    let status = result["status"].as_str().unwrap_or("unknown").to_owned();
    let failed = result["properties"]
        .as_array()
        .map(|properties| {
            properties
                .iter()
                .filter(|property| property["status"] == "failed")
                .filter_map(|property| property["name"].as_str())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    (status, failed)
}

/// Run a campaign completion hook once, after the runner has retained its
/// results. The hook is a `sh` command string with `THESEUS_CAMPAIGN_DIR`,
/// `THESEUS_CAMPAIGN_STATUS`, and the comma-separated
/// `THESEUS_FAILED_PROPERTIES` in its environment, so a webhook curl or a
/// CI step can react without a hosted service. The hook never changes
/// verdicts or evidence: its output streams to stderr and its failure is
/// reported without failing the campaign.
fn notify_campaign_completion(notify: Option<&str>, output: &Path) {
    let Some(command) = notify else {
        return;
    };
    let (status, failed) = campaign_completion_status(output);
    let hook = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("THESEUS_CAMPAIGN_DIR", output)
        .env("THESEUS_CAMPAIGN_STATUS", &status)
        .env("THESEUS_FAILED_PROPERTIES", failed.join(","))
        .status();
    match hook {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("theseus: notification hook failed: {status}"),
        Err(error) => eprintln!("theseus: notification hook cannot start: {error}"),
    }
}

/// Execute a campaign which is deliberately expected to falsify one named
/// property. The command succeeds only after the runner has completed the
/// campaign and retained that exact failed verdict.
pub fn explore_compose_expect_counterexample(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
    property: &str,
) -> Result<PathBuf, ComposeError> {
    let mut plan = load_compose_plan(&path)?;
    let campaign = plan.campaign.as_ref().ok_or_else(|| {
        ComposeError::Invalid("Compose file has no x-theseus.campaign section".to_owned())
    })?;
    if !campaign
        .properties
        .iter()
        .any(|candidate| candidate.name == property)
    {
        return Err(ComposeError::Invalid(format!(
            "campaign has no property named {property:?}"
        )));
    }
    plan.topology_runner = Some(installed_runner_artifact()?);
    let output = output.as_ref().to_path_buf();
    let plan_file = write_temporary_topology_plan(&plan, &output)?;
    let result = execute_topology_expect_counterexample(&plan_file, &output, None, property);
    let _ = fs::remove_file(&plan_file);
    result.map(|()| output)
}

/// Execute a campaign expected to falsify one named property, with the same
/// fixed-budget and guidance overrides as [`explore_compose_with`].
pub fn explore_compose_expect_counterexample_with(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
    property: &str,
    max_runs: Option<u16>,
    guidance: Option<CampaignGuidance>,
    notify: Option<&str>,
    shard: Option<(u16, u16)>,
) -> Result<PathBuf, ComposeError> {
    let mut plan = load_compose_plan(&path)?;
    let campaign = plan.campaign.as_ref().ok_or_else(|| {
        ComposeError::Invalid("Compose file has no x-theseus.campaign section".to_owned())
    })?;
    if !campaign
        .properties
        .iter()
        .any(|candidate| candidate.name == property)
    {
        return Err(ComposeError::Invalid(format!(
            "campaign has no property named {property:?}"
        )));
    }
    let shard = shard.map(validate_campaign_shard).transpose()?;
    if let Some(campaign) = plan.campaign.as_mut() {
        if let Some(max_runs) = max_runs {
            campaign.max_runs = max_runs;
        }
        if let Some(guidance) = guidance {
            campaign.guidance = guidance;
        }
        campaign.shard = shard;
    }
    plan.topology_runner = Some(installed_runner_artifact()?);
    let output = output.as_ref().to_path_buf();
    let plan_file = write_temporary_topology_plan(&plan, &output)?;
    let result = execute_topology_expect_counterexample(&plan_file, &output, None, property);
    let _ = fs::remove_file(&plan_file);
    notify_campaign_completion(notify, &output);
    result.map(|()| output)
}

/// Validate one exploration shard: `index` out of `total` workers, with a
/// bounded worker count so one campaign cannot fan out unbounded KVM load.
fn validate_campaign_shard(shard: (u16, u16)) -> Result<ShardPlan, ComposeError> {
    let (index, total) = shard;
    if total == 0 || total > 64 || index >= total {
        return Err(ComposeError::Invalid(format!(
            "campaign shard must be INDEX/TOTAL with 0 <= INDEX < TOTAL <= 64: {index}/{total}"
        )));
    }
    Ok(ShardPlan { index, total })
}

/// Re-run a recorded topology using its locked service artifacts.
pub fn replay_compose(
    bundle: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| ComposeError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    let plan = bundle.join("replay-plan.json");
    if !plan.is_file() {
        return Err(ComposeError::Invalid(format!(
            "topology replay has no replay-plan.json: {}",
            bundle.display()
        )));
    }
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "replay output already exists: {}",
            output.display()
        )));
    }
    execute_topology(&plan, &output)?;
    Ok(output)
}

/// Reduce one recorded campaign counterexample into a single locked topology
/// replay.  The Linux runner performs the re-executions because only it owns
/// the deterministic VMM and simulated-switch state.
pub fn minimize_compose_campaign(
    bundle: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| ComposeError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    let plan = bundle.join("replay-plan.json");
    if !bundle.join("campaign-result.json").is_file() {
        return Err(ComposeError::Invalid(format!(
            "campaign bundle has no campaign-result.json: {}",
            bundle.display()
        )));
    }
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "minimized output already exists: {}",
            output.display()
        )));
    }
    execute_topology_mode(&plan, &output, Some("--minimize"))?;
    Ok(output)
}

/// Minimize a known campaign failure and treat reproduction of the named
/// counterexample as the successful outcome.
pub fn minimize_compose_campaign_expect_counterexample(
    bundle: impl AsRef<Path>,
    output: impl AsRef<Path>,
    property: &str,
) -> Result<PathBuf, ComposeError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| ComposeError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    let plan = bundle.join("replay-plan.json");
    if !bundle.join("campaign-result.json").is_file() {
        return Err(ComposeError::Invalid(format!(
            "campaign bundle has no campaign-result.json: {}",
            bundle.display()
        )));
    }
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "minimized output already exists: {}",
            output.display()
        )));
    }
    execute_topology_expect_counterexample(&plan, &output, Some("--minimize"), property)?;
    Ok(output)
}

/// Fork one recorded campaign run as a counterfactual experiment: re-execute
/// the recorded schedule of `run` with its recorded `fault` decision replaced
/// by the declared `replace` fault, reusing the deterministic shared prefix,
/// into a fresh campaign directory beside the untouched original.
///
/// The substitution is locked into the forked replay plan like any override,
/// so the forked future records its own provenance and pairs with `theseus
/// compare --forked` against the retained base campaign.
pub fn explore_compose_forked(
    bundle: impl AsRef<Path>,
    run: usize,
    fault: &str,
    replace: &str,
    output: impl AsRef<Path>,
    notify: Option<&str>,
) -> Result<PathBuf, ComposeError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| ComposeError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    if !bundle.join("replay-plan.json").is_file() {
        return Err(ComposeError::Invalid(format!(
            "campaign bundle has no replay-plan.json: {}",
            bundle.display()
        )));
    }
    if !bundle.join("campaign-result.json").is_file() {
        return Err(ComposeError::Invalid(format!(
            "campaign bundle has no campaign-result.json: {}",
            bundle.display()
        )));
    }
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "forked output already exists: {}",
            output.display()
        )));
    }
    let plan = forked_counterfactual_plan(&bundle, run, fault, replace)?;
    // The runner reads the recorded campaign result and resolves relative
    // artifact paths beside its plan file, so the substituted plan is staged
    // next to the output with artifact paths rewritten to their retained
    // bundle locations. The bundle itself is never modified.
    let staging = output.with_extension("counterfactual-plan");
    if staging.exists() {
        return Err(ComposeError::Invalid(format!(
            "counterfactual staging directory already exists: {}",
            staging.display()
        )));
    }
    fs::create_dir_all(&staging).map_err(|source| ComposeError::Read {
        path: staging.clone(),
        source,
    })?;
    let staged_plan = staging.join("replay-plan.json");
    let staged_result = staging.join("campaign-result.json");
    let staged = (|| {
        fs::write(
            &staged_plan,
            serde_json::to_vec_pretty(&plan).map_err(|error| {
                ComposeError::Invalid(format!("cannot encode forked plan: {error}"))
            })?,
        )
        .map_err(|source| ComposeError::Read {
            path: staged_plan.clone(),
            source,
        })?;
        fs::copy(bundle.join("campaign-result.json"), &staged_result).map_err(|source| {
            ComposeError::Read {
                path: bundle.join("campaign-result.json"),
                source,
            }
        })?;
        Ok(())
    })();
    let result = staged.and_then(|()| execute_topology(&staged_plan, &output));
    let _ = fs::remove_dir_all(&staging);
    notify_campaign_completion(notify, &output);
    result.map(|()| output)
}

/// Validate one counterfactual fork against the retained bundle and return
/// the bundle's replay plan with the locked substitution injected. The
/// replaced fault must be one the recorded run selected and the replacement
/// must differ from it; the replacement's existence is resolved against the
/// plan's declared faults by the runner, which owns the fault-naming
/// contract.
fn forked_counterfactual_plan(
    bundle: &Path,
    run: usize,
    fault: &str,
    replace: &str,
) -> Result<serde_json::Value, ComposeError> {
    let result = read_bundle_json(bundle, "campaign-result.json")?;
    let mut plan = read_bundle_json(bundle, "replay-plan.json")?;
    if fault == replace {
        return Err(ComposeError::Invalid(
            "counterfactual replacement must differ from the replaced fault".to_owned(),
        ));
    }
    let recorded_faults = recorded_run_faults(&result, run)?;
    if !recorded_faults.iter().any(|name| name == fault) {
        return Err(ComposeError::Invalid(format!(
            "recorded run {run} does not select fault {fault:?}"
        )));
    }
    if plan
        .get("campaign")
        .map(serde_json::Value::is_null)
        .unwrap_or(true)
    {
        return Err(ComposeError::Invalid(
            "campaign bundle plan has no campaign section".to_owned(),
        ));
    }
    absolutize_artifact_paths(&mut plan, bundle)?;
    plan["campaign"]["counterfactual"] = serde_json::json!({
        "run": run,
        "fault": fault,
        "replace": replace,
    });
    Ok(plan)
}

fn read_bundle_json(bundle: &Path, name: &str) -> Result<serde_json::Value, ComposeError> {
    let path = bundle.join(name);
    let bytes = fs::read(&path).map_err(|source| ComposeError::Read {
        path: path.clone(),
        source,
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ComposeError::Invalid(format!("cannot parse {}: {error}", path.display())))
}

/// The fault decisions one recorded run selected, accepting both the current
/// `faults` list and the legacy single-fault field.
fn recorded_run_faults(
    result: &serde_json::Value,
    run: usize,
) -> Result<Vec<String>, ComposeError> {
    let runs = result
        .get("runs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ComposeError::Invalid("recorded campaign result has no runs".to_owned()))?;
    let run = runs.get(run).ok_or_else(|| {
        ComposeError::Invalid(format!(
            "recorded campaign has no run {run} ({} runs)",
            runs.len()
        ))
    })?;
    let mut names = run
        .get("faults")
        .and_then(serde_json::Value::as_array)
        .map(|faults| {
            faults
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if names.is_empty() {
        if let Some(fault) = run.get("fault").and_then(serde_json::Value::as_str) {
            names.push(fault.to_owned());
        }
    }
    Ok(names)
}

/// Rewrite every locked `{sha256, path}` artifact in a bundle replay plan to
/// its absolute retained location. The runner leaves absolute paths alone, so
/// the staged counterfactual plan keeps consuming the bundle's inputs rather
/// than looking for them beside the forked output. This mirrors the runner's
/// own relative-path rewriting when it writes a replay plan.
fn absolutize_artifact_paths(
    value: &mut serde_json::Value,
    parent: &Path,
) -> Result<(), ComposeError> {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                absolutize_artifact_paths(value, parent)?;
            }
        }
        serde_json::Value::Object(object) => {
            if object
                .get("sha256")
                .is_some_and(serde_json::Value::is_string)
            {
                if let Some(serde_json::Value::String(path)) = object.get_mut("path") {
                    let target = PathBuf::from(path.as_str());
                    if target.is_relative() {
                        *path = fs::canonicalize(parent.join(&target))
                            .map_err(|source| ComposeError::Read {
                                path: target.clone(),
                                source,
                            })?
                            .display()
                            .to_string();
                    }
                }
            } else {
                for value in object.values_mut() {
                    absolutize_artifact_paths(value, parent)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn write_temporary_topology_plan(
    plan: &ComposePlan,
    output: &Path,
) -> Result<PathBuf, ComposeError> {
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "replay output already exists: {}",
            output.display()
        )));
    }
    let plan_file = output.with_extension("topology-plan.json");
    if plan_file.exists() {
        return Err(ComposeError::Invalid(format!(
            "temporary topology plan already exists: {}",
            plan_file.display()
        )));
    }
    fs::write(
        &plan_file,
        serde_json::to_vec_pretty(plan).map_err(|error| {
            ComposeError::Invalid(format!("cannot encode topology plan: {error}"))
        })?,
    )
    .map_err(|source| ComposeError::Read {
        path: plan_file.clone(),
        source,
    })?;
    Ok(plan_file)
}

fn execute_topology(plan: &Path, output: &Path) -> Result<(), ComposeError> {
    execute_topology_mode(plan, output, None)
}

fn execute_topology_mode(
    plan: &Path,
    output: &Path,
    mode: Option<&str>,
) -> Result<(), ComposeError> {
    let status = run_topology(plan, output, mode)?;
    if !status.success() {
        return Err(ComposeError::Invalid(format!(
            "topology runner failed; inspect {}",
            output.display()
        )));
    }
    Ok(())
}

fn execute_topology_expect_counterexample(
    plan: &Path,
    output: &Path,
    mode: Option<&str>,
    property: &str,
) -> Result<(), ComposeError> {
    let status = run_topology(plan, output, mode)?;
    if status.success() {
        return Err(ComposeError::Invalid(format!(
            "campaign passed but property {property:?} was expected to fail; inspect {}",
            output.display()
        )));
    }
    verify_counterexample_result(output, property, mode == Some("--minimize"))
}

fn run_topology(
    plan: &Path,
    output: &Path,
    mode: Option<&str>,
) -> Result<std::process::ExitStatus, ComposeError> {
    let runner: TopologyRunnerPlan = serde_json::from_slice(&fs::read(plan).map_err(|source| {
        ComposeError::Read {
            path: plan.to_path_buf(),
            source,
        }
    })?)
    .map_err(|error| ComposeError::Invalid(format!("cannot parse {}: {error}", plan.display())))?;
    let runner = runner
        .topology_runner
        .as_ref()
        .ok_or_else(|| ComposeError::Invalid("topology replay has no locked executor; replay it with the published runtime that created it".to_owned()))?;
    let runner = verified_runner(runner, plan)?;
    Command::new(&runner)
        .arg("--plan")
        .arg(plan)
        .arg("--output")
        .arg(output)
        .args(mode)
        .status()
        .map_err(|error| {
            ComposeError::Invalid(format!("cannot start {}: {error}", runner.display()))
        })
}

#[derive(Deserialize)]
struct CounterexampleCampaignResult {
    format: String,
    status: String,
    properties: Vec<CounterexamplePropertyResult>,
}

#[derive(Deserialize)]
struct CounterexamplePropertyResult {
    name: String,
    status: String,
}

#[derive(Deserialize)]
struct CounterexampleMinimizationResult {
    property: String,
}

fn verify_counterexample_result(
    output: &Path,
    property: &str,
    minimized: bool,
) -> Result<(), ComposeError> {
    if minimized {
        let path = output.join("minimization.json");
        let result: CounterexampleMinimizationResult =
            serde_json::from_slice(&fs::read(&path).map_err(|source| ComposeError::Read {
                path: path.clone(),
                source,
            })?)
            .map_err(|error| {
                ComposeError::Invalid(format!("cannot parse {}: {error}", path.display()))
            })?;
        if result.property == property && output.join("topology-result.json").is_file() {
            return Ok(());
        }
    } else {
        let path = output.join("campaign-result.json");
        let result: CounterexampleCampaignResult =
            serde_json::from_slice(&fs::read(&path).map_err(|source| ComposeError::Read {
                path: path.clone(),
                source,
            })?)
            .map_err(|error| {
                ComposeError::Invalid(format!("cannot parse {}: {error}", path.display()))
            })?;
        if result.format == "theseus-compose-campaign-result-v1"
            && result.status == "failed"
            && result
                .properties
                .iter()
                .any(|candidate| candidate.name == property && candidate.status == "failed")
        {
            return Ok(());
        }
    }
    Err(ComposeError::Invalid(format!(
        "topology runner failed without retaining the expected counterexample for {property:?}; inspect {}",
        output.display()
    )))
}

#[derive(Deserialize)]
struct TopologyRunnerPlan {
    #[serde(default)]
    topology_runner: Option<ArtifactPlan>,
}

fn installed_runner() -> Result<PathBuf, ComposeError> {
    let runner = std::env::current_exe()
        .map_err(|error| ComposeError::Invalid(format!("cannot locate theseus binary: {error}")))?
        .parent()
        .map(|directory| directory.join("theseus-topology"))
        .ok_or_else(|| {
            ComposeError::Invalid("theseus binary has no parent directory".to_owned())
        })?;
    if !runner.is_file() {
        return Err(ComposeError::Invalid(format!(
            "missing Linux topology runner beside theseus: {}; use a published Linux runtime bundle",
            runner.display()
        )));
    }
    Ok(runner)
}

fn installed_runner_artifact() -> Result<ArtifactPlan, ComposeError> {
    artifact_for_runner(&installed_runner()?)
}

fn verified_runner(artifact: &ArtifactPlan, plan: &Path) -> Result<PathBuf, ComposeError> {
    let path = PathBuf::from(&artifact.path);
    let path = if path.is_absolute() {
        path
    } else {
        plan.parent()
            .ok_or_else(|| {
                ComposeError::Invalid(format!(
                    "topology plan has no parent directory: {}",
                    plan.display()
                ))
            })?
            .join(path)
    };
    let verified = artifact_for_runner(&path)?;
    if verified.sha256 != artifact.sha256 {
        return Err(ComposeError::Invalid(format!(
            "topology runner digest changed: {}",
            path.display()
        )));
    }
    Ok(PathBuf::from(verified.path))
}

fn artifact_for_runner(path: &Path) -> Result<ArtifactPlan, ComposeError> {
    let path = fs::canonicalize(path).map_err(|source| ComposeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = fs::read(&path).map_err(|source| ComposeError::Read {
        path: path.clone(),
        source,
    })?;
    Ok(ArtifactPlan {
        path: path.display().to_string(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tar_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8], mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }

    fn write_docker_image(path: &Path, files: &[(&str, u32)]) {
        let mut layer = tar::Builder::new(Vec::new());
        for (name, mode) in files {
            tar_file(&mut layer, name, b"#!/bin/sh\nexit 0\n", *mode);
        }
        let layer = layer.into_inner().unwrap();
        let manifest =
            br#"[{"Config":"config.json","RepoTags":["test:latest"],"Layers":["layer.tar"]}]"#;
        let config = br#"{"config":{"Entrypoint":["/bin/sh"],"Cmd":["-c","sleep 3600"]}}"#;
        let mut image = tar::Builder::new(Vec::new());
        tar_file(&mut image, "manifest.json", manifest, 0o644);
        tar_file(&mut image, "config.json", config, 0o644);
        tar_file(&mut image, "layer.tar", &layer, 0o644);
        fs::write(path, image.into_inner().unwrap()).unwrap();
    }

    fn fixture(compose: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for service in ["api", "worker", "auditor"] {
            let root = directory.path().join(service);
            fs::create_dir_all(root.join("runtime")).unwrap();
            fs::create_dir_all(root.join("guest")).unwrap();
            fs::write(root.join("runtime/firecracker"), b"firecracker").unwrap();
            #[cfg(unix)]
            fs::set_permissions(
                root.join("runtime/firecracker"),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            fs::write(root.join("guest/vmlinux"), b"kernel").unwrap();
            fs::write(root.join("guest/initramfs.cpio"), b"initramfs").unwrap();
            fs::write(
                root.join("theseus.toml"),
                "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\n[guest]\nkernel = 'guest/vmlinux'\ninitramfs = 'guest/initramfs.cpio'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
            )
            .unwrap();
        }
        fs::write(directory.path().join("compose.yaml"), compose).unwrap();
        directory
    }

    #[test]
    fn compose_rejects_a_single_service_ready_checkpoint_flag() {
        let directory =
            fixture("services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [test]\nnetworks:\n  test: {}\n");
        let manifest = directory.path().join("api/theseus.toml");
        let text = fs::read_to_string(&manifest)
            .unwrap()
            .replace("seed = 1", "seed = 1\nreplay_start = 'ready_checkpoint'");
        fs::write(
            &manifest,
            format!("{text}\n[[events]]\nwhen = 'ready'\ndata = '41'\n"),
        )
        .unwrap();
        assert!(load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("manages topology checkpoints"));
    }

    #[test]
    fn compose_accepts_explicit_topology_checkpoint_and_requires_all_clocks() {
        let directory = fixture("services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  replay_start: ready_checkpoint\n");
        let path = directory.path().join("compose.yaml");
        let plan = load_compose_plan(&path).unwrap();
        assert_eq!(
            plan.replay_start,
            crate::manifest::ReplayStart::ReadyCheckpoint
        );
        let manifest = directory.path().join("api/theseus.toml");
        let text = fs::read_to_string(&manifest).unwrap();
        fs::write(&manifest, text.split("[run.virtual_time]").next().unwrap()).unwrap();
        assert!(load_compose_plan(&path)
            .unwrap_err()
            .to_string()
            .contains("virtual time on every service"));
    }

    #[test]
    fn runtime_witness_tutorial_is_a_valid_checkpoint_plan() {
        let compose = include_str!("../../docs/tutorials/11-certify-runtime/compose.yaml")
            .replace("  service:", "  api:")
            .replace("service/theseus.toml", "api/theseus.toml");
        let directory = fixture(&compose);
        let manifest = include_str!("../../docs/tutorials/11-certify-runtime/service/theseus.toml")
            .replace("guest/initramfs.cpio.gz", "guest/initramfs.cpio");
        fs::write(directory.path().join("api/theseus.toml"), manifest).unwrap();
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.replay_start,
            crate::manifest::ReplayStart::ReadyCheckpoint
        );
        assert_eq!(
            plan.services["api"].run.events[0].data_hex,
            "66696e6973680a"
        );
        assert_eq!(
            plan.services["api"].run.events[0].checkpoint.as_deref(),
            Some("finished")
        );
        assert_eq!(plan.services["api"].run.events.len(), 1);
        assert_eq!(plan.services["api"].run.storage.len(), 1);
    }

    fn image_fixture(compose: &str, files: &[(&str, u32)]) -> tempfile::TempDir {
        let directory = fixture(compose);
        fs::write(
            directory.path().join("api/runtime/theseus-image"),
            b"adapter",
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            directory.path().join("api/runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        write_docker_image(&directory.path().join("api/service.tar"), files);
        fs::write(
            directory.path().join("api/theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n[container_service.ready]\nurl = 'http://127.0.0.1:8080/health'\n",
        )
        .unwrap();
        directory
    }

    /// Both services image-backed with a container_service contract, so the
    /// standard profile can propose custom candidates for either.
    fn two_image_fixture(compose: &str) -> tempfile::TempDir {
        let directory = image_fixture(compose, &[]);
        for service in ["worker"] {
            fs::write(
                directory.path().join(service).join("runtime/theseus-image"),
                b"adapter",
            )
            .unwrap();
            #[cfg(unix)]
            fs::set_permissions(
                directory.path().join(service).join("runtime/theseus-image"),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            write_docker_image(&directory.path().join(service).join("service.tar"), &[]);
            fs::write(
                directory.path().join(service).join("theseus.toml"),
                "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n[container_service.ready]\nurl = 'http://127.0.0.1:8081/health'\n",
            )
            .unwrap();
        }
        directory
    }

    #[test]
    fn standard_fault_profile_generates_custom_candidates_from_declared_commands() {
        let directory = two_image_fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: write\n        service: api\n        shell: {command: [/work/write]}\n      - name: rewrite\n        service: api\n        shell: {command: [/work/write]}\n      - name: read\n        service: worker\n        shell: {command: [/work/read]}\n    fault_profile: standard\n",
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.unwrap();
        let customs = campaign
            .faults
            .iter()
            .filter(|fault| matches!(fault.kind, CampaignFaultKind::Custom))
            .collect::<Vec<_>>();
        // Two distinct commands (api's write argv is deduplicated across the
        // write and rewrite operations), each proposed at all three eligible
        // boundaries, including the other service's.
        assert_eq!(customs.len(), 6);
        let mut pairs = customs
            .iter()
            .map(|fault| {
                (
                    fault.service.clone().unwrap(),
                    fault.after.clone().unwrap(),
                    fault.command.clone().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (
                    "api".to_owned(),
                    "read".to_owned(),
                    vec!["/work/write".to_owned()]
                ),
                (
                    "api".to_owned(),
                    "rewrite".to_owned(),
                    vec!["/work/write".to_owned()]
                ),
                (
                    "api".to_owned(),
                    "write".to_owned(),
                    vec!["/work/write".to_owned()]
                ),
                (
                    "worker".to_owned(),
                    "read".to_owned(),
                    vec!["/work/read".to_owned()]
                ),
                (
                    "worker".to_owned(),
                    "rewrite".to_owned(),
                    vec!["/work/read".to_owned()]
                ),
                (
                    "worker".to_owned(),
                    "write".to_owned(),
                    vec!["/work/read".to_owned()]
                ),
            ]
        );
        // Generated customs are optional search choices like declared ones.
        assert!(customs.iter().all(|fault| !fault.required));
    }

    #[test]
    fn standard_fault_profile_skips_generated_duplicates_of_declared_faults() {
        let directory = two_image_fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: write\n        service: api\n        shell: {command: [/work/write]}\n      - name: read\n        service: worker\n        shell: {command: [/work/read]}\n    fault_profile: standard\n    faults:\n      - kind: custom\n        service: api\n        after: write\n        command: [/work/write]\n",
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.unwrap();
        // The declared fault restates one generated candidate exactly; the
        // redundant generated copy is skipped, not rejected, and the
        // declared one stays first.
        let customs = campaign
            .faults
            .iter()
            .filter(|fault| matches!(fault.kind, CampaignFaultKind::Custom))
            .collect::<Vec<_>>();
        assert_eq!(customs.len(), 4);
        assert_eq!(customs[0].service.as_deref(), Some("api"));
        assert_eq!(customs[0].after.as_deref(), Some("write"));
        assert_eq!(
            customs[0].command.as_deref(),
            Some(&["/work/write".to_owned()][..])
        );
    }

    #[test]
    fn locks_service_artifacts_and_links() {
        let directory = fixture(
            "name: example\nservices:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.format, "theseus-compose-plan-v1");
        assert_eq!(plan.networks["backplane"], ["api", "worker"]);
        assert_eq!(plan.services["api"].run.guest.kernel.sha256.len(), 64);
    }

    #[test]
    fn discovers_antithesis_test_template_commands_from_images() {
        let directory = image_fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    test_template: main\n    max_parallel_commands: 2\n    max_operations_per_run: 6\n    faults: []\n",
            &[
                ("opt/antithesis/test/v1/main/first_prepare.sh", 0o755),
                (
                    "opt/antithesis/test/v1/main/parallel_driver_write.sh",
                    0o755,
                ),
                ("opt/antithesis/test/v1/main/eventually_check.sh", 0o755),
                ("opt/antithesis/test/v1/main/helper_library.sh", 0o755),
            ],
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.unwrap();
        assert_eq!(campaign.test_template.as_deref(), Some("main"));
        assert!(campaign.test_templates.is_empty());
        assert_eq!(campaign.operations.len(), 6);
        assert_eq!(
            campaign
                .operations
                .iter()
                .filter(|operation| operation.shell_phase == Some(ComposeShellPhase::Launch))
                .count(),
            2
        );
        assert!(campaign.operations.iter().all(|operation| operation
            .test_command_path
            .as_deref()
            .is_some_and(|path| path.starts_with("/opt/antithesis/test/v1/main/"))));
        assert!(!campaign
            .operations
            .iter()
            .any(|operation| operation.name.contains("helper")));
    }

    #[test]
    fn standard_fault_profile_expands_only_ordinary_template_boundaries() {
        let directory = image_fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    test_template: main\n    max_parallel_commands: 1\n    max_operations_per_run: 4\n    fault_profile: standard\n",
            &[
                ("opt/antithesis/test/v1/main/first_prepare", 0o755),
                (
                    "opt/antithesis/test/v1/main/parallel_driver_write",
                    0o755,
                ),
                ("opt/antithesis/test/v1/main/eventually_check", 0o755),
            ],
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert!(
            plan.services["api"]
                .run
                .container_service
                .as_ref()
                .unwrap()
                .campaign
        );
        let campaign = plan.campaign.unwrap();
        assert_eq!(campaign.fault_profile, Some(ComposeFaultProfile::Standard));
        assert_eq!(campaign.faults.len(), 12);
        assert!(campaign.faults.iter().all(|fault| fault
            .after
            .as_deref()
            .is_some_and(|after| after.contains("start"))));
        // The profile re-runs the service's own parallel-write command as a
        // custom candidate at every eligible barrier.
        let customs = campaign
            .faults
            .iter()
            .filter(|fault| matches!(fault.kind, CampaignFaultKind::Custom))
            .collect::<Vec<_>>();
        assert_eq!(customs.len(), 1);
        assert_eq!(customs[0].service.as_deref(), Some("api"));
        assert_eq!(
            customs[0].command.as_deref(),
            Some(&["/opt/antithesis/test/v1/main/parallel_driver_write".to_owned()][..])
        );
        assert_eq!(
            campaign
                .faults
                .iter()
                .filter(|fault| matches!(fault.kind, CampaignFaultKind::ServiceKill))
                .count(),
            1
        );
        let throttles = campaign
            .faults
            .iter()
            .filter(|fault| matches!(fault.kind, CampaignFaultKind::CpuThrottle))
            .collect::<Vec<_>>();
        assert_eq!(throttles.len(), 1);
        assert_eq!(throttles[0].duration_rounds, Some(16));
        assert_eq!(throttles[0].every_n_rounds, Some(4));
        let rates = campaign
            .faults
            .iter()
            .filter(|fault| matches!(fault.kind, CampaignFaultKind::ClockRate))
            .collect::<Vec<_>>();
        assert_eq!(rates.len(), 1);
        assert_eq!(rates[0].rate, Some(4));
        assert_eq!(rates[0].duration_rounds, Some(32));
        let links = campaign
            .faults
            .iter()
            .filter(|fault| matches!(fault.kind, CampaignFaultKind::LinkFault))
            .collect::<Vec<_>>();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].drop_ppm, Some(100_000));
        assert_eq!(links[0].latency_rounds, Some(2));
        assert_eq!(links[0].mtu_bytes, Some(1_200));
        let clogs = campaign
            .faults
            .iter()
            .filter(|fault| matches!(fault.kind, CampaignFaultKind::LinkClog))
            .collect::<Vec<_>>();
        assert_eq!(clogs.len(), 2);
        assert_eq!(clogs[0].latency_rounds, Some(64));
        assert_eq!(clogs[0].from.as_deref(), Some("api"));
        assert_eq!(clogs[0].to.as_deref(), Some("worker"));
    }

    #[test]
    fn discovers_and_scopes_every_image_test_template() {
        let directory = image_fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_parallel_commands: 1\n    max_operations_per_run: 4\n    faults: []\n",
            &[
                (
                    "opt/antithesis/test/v1/lost-update/parallel_driver_write",
                    0o755,
                ),
                (
                    "opt/antithesis/test/v1/lost-update/finally_check",
                    0o755,
                ),
                (
                    "opt/antithesis/test/v1/smoke/singleton_driver_health",
                    0o755,
                ),
                ("opt/antithesis/test/v1/smoke/finally_check", 0o755),
            ],
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.unwrap();
        assert_eq!(campaign.test_template, None);
        assert_eq!(campaign.test_templates, ["lost-update", "smoke"]);
        assert_eq!(campaign.operations.len(), 5);
        assert!(campaign.operations.iter().all(|operation| operation
            .test_template
            .as_ref()
            .is_some_and(|template| campaign.test_templates.contains(template))));
        assert!(campaign
            .operations
            .iter()
            .any(|operation| operation.name.starts_with("lost-update_api_")));
        assert!(campaign
            .operations
            .iter()
            .any(|operation| operation.name.starts_with("smoke_api_")));
    }

    #[test]
    fn limits_image_discovery_to_explicit_test_templates() {
        let directory = image_fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    test_templates: [smoke]\n    faults: []\n",
            &[
                (
                    "opt/antithesis/test/v1/ignored/parallel_driver_write",
                    0o755,
                ),
                ("opt/antithesis/test/v1/smoke/anytime_health", 0o755),
            ],
        );

        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert_eq!(campaign.test_template.as_deref(), Some("smoke"));
        assert!(campaign.test_templates.is_empty());
        assert!(campaign.operations.iter().all(|operation| operation
            .test_command_path
            .as_deref()
            .is_some_and(|path| path.contains("/smoke/"))));
    }

    #[test]
    fn locks_llvm_coverage_manifests_and_symbols() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      coverage:\n        - manifest: api/coverage.json\n          symbols: api/symbols\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let digest = "0123456789abcdef".repeat(4);
        fs::create_dir(directory.path().join("api/symbols")).unwrap();
        fs::write(
            directory.path().join("api/symbols/api.debug"),
            [b"\x7fELF".as_slice(), digest.as_bytes()].concat(),
        )
        .unwrap();
        fs::write(
            directory.path().join("api/coverage.json"),
            format!(
                "{{\"format\":\"theseus-llvm-coverage-build-v1\",\"coverage\":\"edges\",\"language\":\"c++\",\"process\":\"api\",\"module\":\"command\",\"build_sha256\":\"{digest}\",\"gnu_build_id\":\"0123456789abcdef\",\"symbols\":\"api.debug\"}}"
            ),
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let coverage = &plan.services["api"].coverage[0];
        assert_eq!(coverage.format, "theseus-llvm-coverage-build-v1");
        assert_eq!(coverage.coverage, "edges");
        assert_eq!(coverage.language, "c++");
        assert_eq!(coverage.module, "command");
        assert_eq!(coverage.build_sha256, digest);
        assert_eq!(coverage.gnu_build_id.as_deref(), Some("0123456789abcdef"));
        assert_eq!(coverage.manifest.sha256.len(), 64);
        assert_eq!(coverage.symbols.sha256.len(), 64);

        fs::write(
            directory.path().join("api/symbols/api.debug"),
            b"\x7fELFwrong build",
        )
        .unwrap();
        assert!(load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("does not match build"));
    }

    #[test]
    fn locks_go_coverage_manifests_and_symbols() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      coverage:\n        - manifest: api/go-coverage.json\n          symbols: api/go-symbols\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let digest = "fedcba9876543210".repeat(4);
        fs::create_dir(directory.path().join("api/go-symbols")).unwrap();
        fs::write(
            directory.path().join("api/go-symbols/api.debug"),
            [b"\x7fELF".as_slice(), digest.as_bytes()].concat(),
        )
        .unwrap();
        fs::write(
            directory.path().join("api/go-coverage.json"),
            format!(
                "{{\"format\":\"theseus-go-coverage-build-v1\",\"coverage\":\"blocks\",\"language\":\"go\",\"process\":\"api\",\"module\":\"command\",\"build_sha256\":\"{digest}\",\"symbols\":\"api.debug\"}}"
            ),
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let coverage = &plan.services["api"].coverage[0];
        assert_eq!(coverage.format, "theseus-go-coverage-build-v1");
        assert_eq!(coverage.coverage, "blocks");
        assert_eq!(coverage.language, "go");
        assert_eq!(coverage.build_sha256, digest);
        assert_eq!(coverage.gnu_build_id, None);
        assert_eq!(coverage.manifest.sha256.len(), 64);
        assert_eq!(coverage.symbols.sha256.len(), 64);
    }

    #[test]
    fn normalizes_campaign_fault_windows_and_quiet_windows() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: write\n        input: 'write\\n'\n      - name: read\n        input: 'read\\n'\n      - name: verify\n        input: 'verify\\n'\n    faults:\n      - kind: partition\n        network: backplane\n        after: write\n        until: verify\n    quiet:\n      - before: read\n",
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.unwrap();
        assert_eq!(campaign.faults.len(), 1);
        assert_eq!(campaign.faults[0].after.as_deref(), Some("write"));
        assert_eq!(campaign.faults[0].until.as_deref(), Some("verify"));
        assert_eq!(campaign.quiet.len(), 1);
        assert_eq!(campaign.quiet[0].before, "read");
    }

    #[test]
    fn rejects_malformed_fault_windows_and_quiet_windows() {
        let base = "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: write\n        input: 'write\\n'\n      - name: read\n        input: 'read\\n'\n    ";
        let case = |campaign_tail: &str| {
            let directory = fixture(&format!("{base}{campaign_tail}"));
            load_compose_plan(directory.path().join("compose.yaml")).unwrap_err()
        };

        // Recovery kinds, lifecycle faults, and custom faults have no window.
        let error = case("faults:\n      - kind: heal\n        network: backplane\n        after: write\n        until: read\n");
        assert!(
            error
                .to_string()
                .contains("have no automatic recovery and accept no until window"),
            "{error}"
        );
        let error = case("faults:\n      - kind: restart\n        service: api\n        at_round: 2\n        until: read\n");
        assert!(
            error
                .to_string()
                .contains("have no automatic recovery and accept no until window"),
            "{error}"
        );

        // A window must close after it opens, at a known operation.
        let error = case("faults:\n      - kind: partition\n        network: backplane\n        after: write\n        until: write\n");
        assert!(
            error
                .to_string()
                .contains("must close at an operation after its trigger"),
            "{error}"
        );
        let error = case("faults:\n      - kind: partition\n        network: backplane\n        after: write\n        until: ghost\n");
        assert!(
            error.to_string().contains("references unknown operation"),
            "{error}"
        );

        // Quiet windows name known operations once each.
        let error = case("quiet:\n      - before: ghost\n");
        assert!(
            error.to_string().contains("references unknown operation"),
            "{error}"
        );
        let error = case("quiet:\n      - before: read\n      - before: read\n");
        assert!(
            error.to_string().contains("same quiet window twice"),
            "{error}"
        );
        let error = case("quiet:\n      - before: read\n        at: write\n");
        assert!(error.to_string().contains("unknown field `at`"), "{error}");
    }
    #[test]
    fn locks_java_coverage_manifests_and_symbols() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      coverage:\n        - manifest: api/java-coverage.json\n          symbols: api/java-symbols\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let digest = "0f1e2d3c4b5a6978".repeat(4);
        fs::create_dir(directory.path().join("api/java-symbols")).unwrap();
        fs::write(
            directory.path().join("api/java-symbols/app.debug"),
            format!(
                r#"{{"format":"theseus-java-coverage-symbols-v1","build_sha256":"{digest}","classes":[{{"class":"com/example/App","offset":"0x0123456789abcdef","source":"com/example/App.java"}}]}}"#
            ),
        )
        .unwrap();
        fs::write(
            directory.path().join("api/java-coverage.json"),
            format!(
                "{{\"format\":\"theseus-java-coverage-build-v1\",\"coverage\":\"classes\",\"language\":\"java\",\"process\":\"api\",\"module\":\"app\",\"build_sha256\":\"{digest}\",\"symbols\":\"app.debug\"}}"
            ),
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let coverage = &plan.services["api"].coverage[0];
        assert_eq!(coverage.format, "theseus-java-coverage-build-v1");
        assert_eq!(coverage.coverage, "classes");
        assert_eq!(coverage.language, "java");
        assert_eq!(coverage.module, "app");
        assert_eq!(coverage.build_sha256, digest);
        assert_eq!(coverage.gnu_build_id, None);
        assert_eq!(coverage.manifest.sha256.len(), 64);
        assert_eq!(coverage.symbols.sha256.len(), 64);

        // A symbol map describing a different build is rejected, and so is
        // a map with no classes.
        fs::write(
            directory.path().join("api/java-symbols/app.debug"),
            format!(
                r#"{{"format":"theseus-java-coverage-symbols-v1","build_sha256":"{digest}","classes":[]}}"#
            ),
        )
        .unwrap();
        assert!(load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("does not describe build"));
    }

    #[test]
    fn locks_short_and_conditional_compose_dependencies() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    depends_on: [worker]\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    depends_on:\n      auditor:\n        condition: service_started\n    networks: [backplane]\n  auditor:\n    x-theseus:\n      manifest: auditor/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.services["api"].depends_on.len(), 1);
        assert_eq!(plan.services["api"].depends_on[0].service, "worker");
        assert_eq!(
            plan.services["worker"].depends_on[0].condition,
            DependencyCondition::ServiceStarted
        );
    }

    #[test]
    fn locks_literal_compose_environment_without_host_inheritance() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    environment: {MODE: campaign, RETRIES: '3'}\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    environment: [ROLE=worker]\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.services["api"].environment["MODE"], "campaign");
        assert_eq!(plan.services["api"].environment["RETRIES"], "3");
        assert_eq!(plan.services["worker"].environment["ROLE"], "worker");

        let inherited = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    environment: [HOST_VALUE]\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        assert!(load_compose_plan(inherited.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("host-environment inheritance"));
    }

    #[test]
    fn locks_compose_resource_limits_into_the_vm_contract() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    cpus: 2\n    mem_limit: 256M\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    deploy:\n      resources:\n        limits:\n          cpus: '2'\n          memory: 192MiB\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.services["api"].run.run.vcpu_count, 2);
        assert_eq!(plan.services["api"].run.run.mem_size_mib, 256);
        assert_eq!(plan.services["worker"].run.run.vcpu_count, 2);
        assert_eq!(plan.services["worker"].run.run.mem_size_mib, 192);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["services"]["api"]["run"]["run"]["mem_size_mib"], 256);

        let invalid = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    cpus: '0.5'\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        assert!(load_compose_plan(invalid.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("whole number"));
    }

    #[test]
    fn locks_literal_compose_env_files_with_explicit_environment_precedence() {
        let directory = fixture(
            "services:\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    env_file: [./base.env, ./override.env]\n    environment:\n      MODE: explicit\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        fs::write(
            directory.path().join("base.env"),
            "# locked locally\nMODE=base\nROLE=worker\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("override.env"),
            "MODE=file-override\nRETRIES=3\n",
        )
        .unwrap();
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.services["worker"].environment["MODE"], "explicit");
        assert_eq!(plan.services["worker"].environment["ROLE"], "worker");
        assert_eq!(plan.services["worker"].environment["RETRIES"], "3");

        let interpolated = fixture(
            "services:\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    env_file: ./worker.env\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        fs::write(
            interpolated.path().join("worker.env"),
            "MODE=${HOST_MODE}\n",
        )
        .unwrap();
        assert!(load_compose_plan(interpolated.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("interpolation"));
    }

    #[test]
    fn locks_compose_host_identity_without_host_name_resolution() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    hostname: api.local\n    extra_hosts:\n      cache.local: 10.9.0.7\n      telemetry.local: '2001:db8::7'\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    extra_hosts: [cache.local=10.9.0.8]\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        for service in ["api", "worker"] {
            let root = directory.path().join(service);
            fs::write(root.join("runtime/theseus-image"), b"adapter").unwrap();
            #[cfg(unix)]
            fs::set_permissions(
                root.join("runtime/theseus-image"),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            fs::write(root.join("guest/image.tar"), b"image").unwrap();
            fs::write(
                root.join("theseus.toml"),
                "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/image.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
            )
            .unwrap();
        }
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.services["api"].hostname.as_deref(), Some("api.local"));
        assert_eq!(plan.services["api"].extra_hosts["cache.local"], "10.9.0.7");
        assert_eq!(
            plan.services["api"].extra_hosts["telemetry.local"],
            "2001:db8::7"
        );
        assert_eq!(
            plan.services["worker"].extra_hosts["cache.local"],
            "10.9.0.8"
        );

        let invalid = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    extra_hosts: [cache.local=not-an-ip]\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        assert!(load_compose_plan(invalid.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("must be an IP address"));
    }

    #[test]
    fn locks_literal_image_launch_without_a_shell_or_host_paths() {
        let launch = image_launch_plan(
            "worker",
            Some(vec![".".to_owned()]),
            Some(vec![
                "/bin/busybox".to_owned(),
                "httpd".to_owned(),
                "-f".to_owned(),
            ]),
            Some("/site".to_owned()),
            Some("1000:1001".to_owned()),
            true,
            vec!["/tmp".to_owned()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(launch.command, Some(vec![".".to_owned()]));
        assert_eq!(launch.entrypoint.as_ref().unwrap()[0], "/bin/busybox");
        assert_eq!(launch.working_dir.as_deref(), Some("/site"));
        assert_eq!(
            launch.user.as_ref().map(|user| (user.uid, user.gid)),
            Some((1000, 1001))
        );
        assert!(launch.read_only);
        assert_eq!(launch.tmpfs, ["/tmp"]);

        assert!(image_launch_plan(
            "worker",
            None,
            Some(vec!["busybox".to_owned()]),
            None,
            None,
            false,
            Vec::new()
        )
        .unwrap_err()
        .to_string()
        .contains("absolute path"));
        assert!(image_launch_plan(
            "worker",
            None,
            None,
            Some("relative".to_owned()),
            None,
            false,
            Vec::new()
        )
        .unwrap_err()
        .to_string()
        .contains("absolute path"));
        assert!(image_launch_plan(
            "worker",
            None,
            None,
            None,
            Some("app".to_owned()),
            false,
            Vec::new()
        )
        .unwrap_err()
        .to_string()
        .contains("numeric uid:gid"));
    }

    #[test]
    fn locks_standard_compose_cmd_healthchecks() {
        let healthcheck = image_healthcheck_plan(
            "worker",
            Some(ComposeHealthcheck {
                test: ComposeHealthcheckTest::Command(vec![
                    "CMD".to_owned(),
                    "/bin/busybox".to_owned(),
                    "true".to_owned(),
                ]),
                interval: Some("500ms".to_owned()),
                retries: Some(4),
                start_period: Some("2s".to_owned()),
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            healthcheck.command,
            ["/bin/busybox", "true"].map(str::to_owned)
        );
        assert_eq!(healthcheck.interval_millis, 500);
        assert_eq!(healthcheck.retries, 4);
        assert_eq!(healthcheck.start_period_millis, 2_000);
        assert!(image_healthcheck_plan(
            "worker",
            Some(ComposeHealthcheck {
                test: ComposeHealthcheckTest::Command(vec![
                    "CMD-SHELL".to_owned(),
                    "true".to_owned(),
                ]),
                interval: None,
                retries: None,
                start_period: None,
            }),
        )
        .unwrap_err()
        .to_string()
        .contains("CMD-SHELL"));
    }

    #[test]
    fn accepts_service_healthy_for_an_image_compose_healthcheck() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    depends_on:\n      worker:\n        condition: service_healthy\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    healthcheck:\n      test: [CMD, /bin/busybox, /bin/true]\n      interval: 1s\n      retries: 2\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let worker = directory.path().join("worker");
        fs::write(worker.join("runtime/theseus-image"), b"adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            worker.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(worker.join("guest/image.tar"), b"image").unwrap();
        fs::write(
            worker.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/image.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
        )
        .unwrap();
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.services["api"].depends_on[0].condition,
            DependencyCondition::ServiceHealthy
        );
        assert_eq!(
            plan.services["worker"]
                .healthcheck
                .as_ref()
                .unwrap()
                .command,
            ["/bin/busybox", "/bin/true"].map(str::to_owned)
        );
    }

    #[test]
    fn locks_read_only_compose_configs_without_host_paths() {
        let definitions = BTreeMap::from([("settings".to_owned(), b"mode=campaign\n".to_vec())]);
        let configs = image_config_plan(
            "worker",
            vec![ComposeServiceConfig::Mount(ComposeConfigMount {
                source: "settings".to_owned(),
                target: Some("/etc/worker.conf".to_owned()),
            })],
            &definitions,
        )
        .unwrap();
        assert_eq!(configs[0].target, "/etc/worker.conf");
        assert_eq!(configs[0].data, b"mode=campaign\n");
        assert!(image_config_plan(
            "worker",
            vec![ComposeServiceConfig::Name("missing".to_owned())],
            &definitions,
        )
        .unwrap_err()
        .to_string()
        .contains("unknown config"));
    }

    #[test]
    fn locks_compose_secrets_at_the_standard_root_only_target() {
        let definitions = BTreeMap::from([("token".to_owned(), b"secret\n".to_vec())]);
        let secrets = image_secret_plan(
            "worker",
            vec![ComposeServiceConfig::Name("token".to_owned())],
            &definitions,
        )
        .unwrap();
        assert_eq!(secrets[0].target, "/run/secrets/token");
        assert_eq!(secrets[0].data, b"secret\n");
        assert!(image_secret_plan(
            "worker",
            vec![ComposeServiceConfig::Name("missing".to_owned())],
            &definitions,
        )
        .unwrap_err()
        .to_string()
        .contains("unknown secret"));
    }

    #[test]
    fn locks_a_writable_compose_bind_volume_without_a_host_mount() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    volumes:\n      - ./worker/data:/var/lib/worker\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let api = directory.path().join("api");
        let data = directory.path().join("worker/data/state");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("value"), b"seeded\n").unwrap();
        fs::write(api.join("runtime/theseus-image"), b"adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            api.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(api.join("guest/image.tar"), b"image").unwrap();
        fs::write(
            api.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/image.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let volume = &plan.services["api"].volumes[0];
        assert_eq!(volume.target, "/var/lib/worker");
        assert_eq!(
            volume.directories,
            ["/var/lib/worker", "/var/lib/worker/state"]
        );
        assert_eq!(volume.files[0].target, "/var/lib/worker/state/value");
        assert_eq!(volume.files[0].data, b"seeded\n");
        assert!(image_volume_plan(
            "api",
            vec![ComposeServiceVolume::Short(
                "data:/var/lib/worker".to_owned()
            )],
            directory.path(),
        )
        .unwrap_err()
        .to_string()
        .contains("must start with ./"));
    }

    #[test]
    fn records_compose_launch_on_an_image_service_only() {
        let directory = fixture(
            "configs:\n  worker_settings:\n    file: worker.conf\nsecrets:\n  worker_token:\n    file: token\nservices:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    command: [--serve]\n    entrypoint: [/bin/worker]\n    working_dir: /srv\n    configs:\n      - source: worker_settings\n        target: /etc/worker.conf\n    secrets: [worker_token]\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let api = directory.path().join("api");
        fs::write(directory.path().join("worker.conf"), b"mode=campaign\n").unwrap();
        fs::write(directory.path().join("token"), b"secret\n").unwrap();
        fs::write(api.join("runtime/theseus-image"), b"adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            api.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(api.join("guest/image.tar"), b"image").unwrap();
        fs::write(
            api.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/image.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
        )
        .unwrap();
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let launch = plan.services["api"].launch.as_ref().unwrap();
        assert_eq!(
            launch.command.as_ref().unwrap(),
            &vec!["--serve".to_owned()]
        );
        assert_eq!(
            launch.entrypoint.as_ref().unwrap(),
            &vec!["/bin/worker".to_owned()]
        );
        assert_eq!(launch.working_dir.as_deref(), Some("/srv"));
        assert_eq!(plan.services["api"].configs[0].target, "/etc/worker.conf");
        assert_eq!(plan.services["api"].configs[0].data, b"mode=campaign\n");
        assert_eq!(
            plan.services["api"].secrets[0].target,
            "/run/secrets/worker_token"
        );
        assert_eq!(plan.services["api"].secrets[0].data, b"secret\n");

        let raw_guest = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    command: [--serve]\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        assert!(load_compose_plan(raw_guest.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("no guest.image"));
    }

    #[test]
    fn rejects_unknown_and_cyclic_compose_dependencies() {
        let unknown = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    depends_on: [missing]\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        assert!(load_compose_plan(unknown.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("unknown service"));

        let cyclic = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    depends_on: [worker]\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    depends_on: [api]\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        assert!(load_compose_plan(cyclic.path().join("compose.yaml"))
            .unwrap_err()
            .to_string()
            .contains("contains a cycle"));
    }

    #[test]
    fn locks_a_declared_http_operation_for_an_image_campaign_driver() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: create\n        http:\n          method: post\n          url: http://127.0.0.1:8080/items\n          body: item\n          expect_status: 201\n          body_contains: created\n    faults: []\n",
        );
        let root = directory.path().join("api");
        fs::write(root.join("runtime/theseus-image"), b"image adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            root.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(root.join("guest/service.tar"), b"image").unwrap();
        fs::write(
            root.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n[container_service.ready]\nurl = 'http://127.0.0.1:8080/health'\n",
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert!(
            plan.services["api"]
                .run
                .container_service
                .as_ref()
                .unwrap()
                .campaign
        );
        let input = &plan.campaign.as_ref().unwrap().operations[0].inputs[0].input_hex;
        let bytes = input
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        let command = String::from_utf8(bytes).unwrap();
        assert!(command.starts_with("THES:HTTP:operation:"));
        assert!(command.contains("\"method\":\"post\""));
        assert!(command.contains("\"expect_status\":201"));
    }

    #[test]
    fn locks_a_declared_grpc_health_operation_for_an_image_campaign_driver() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: check_health\n        grpc_health:\n          url: http://127.0.0.1:50051\n          service: example.Api\n          expect_status: serving\n    faults: []\n",
        );
        let root = directory.path().join("api");
        fs::write(root.join("runtime/theseus-image"), b"image adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            root.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(root.join("guest/service.tar"), b"image").unwrap();
        fs::write(
            root.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n[container_service.grpc_ready]\nurl = 'http://127.0.0.1:50051'\n",
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert!(
            plan.services["api"]
                .run
                .container_service
                .as_ref()
                .unwrap()
                .campaign
        );
        let input = &plan.campaign.as_ref().unwrap().operations[0].inputs[0].input_hex;
        let bytes = input
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        let command = String::from_utf8(bytes).unwrap();
        assert!(command.starts_with("THES:GRPC:operation:"));
        assert!(command.contains("\"service\":\"example.Api\""));
        assert!(command.contains("\"expect_status\":\"serving\""));
    }

    #[test]
    fn locks_a_declared_shell_operation_for_an_image_campaign_driver() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: read_health\n        shell:\n          command: [/bin/cat, /health]\n          output_contains: ok\n          output_json: true\n          environment: {CHECK_MODE: full}\n          choices: {mode: 2, retry: 2}\n          thread_schedule: [0, 1, 2]\n    faults: []\n",
        );
        let root = directory.path().join("api");
        fs::write(root.join("runtime/theseus-image"), b"image adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            root.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(root.join("guest/service.tar"), b"image").unwrap();
        fs::write(
            root.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n[container_service.ready]\nurl = 'http://127.0.0.1:8080/health'\n",
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert!(
            plan.services["api"]
                .run
                .container_service
                .as_ref()
                .unwrap()
                .campaign
        );
        let operation = &plan.campaign.as_ref().unwrap().operations[0];
        assert_eq!(
            plan.campaign.as_ref().unwrap().guidance,
            CampaignGuidance::Unified
        );
        assert_eq!(operation.thread_schedule, [0, 1, 2]);
        assert_eq!(
            operation.choice_bounds,
            BTreeMap::from([("mode".to_owned(), 2), ("retry".to_owned(), 2)])
        );
        assert_eq!(operation.inputs.len(), 4);
        assert_eq!(operation.inputs[0].name, "mode-0+retry-0");
        assert_eq!(operation.inputs[3].name, "mode-1+retry-1");
        assert_eq!(operation.inputs[3].choices["mode"], 1);
        assert_eq!(operation.inputs[3].choices["retry"], 1);
        assert_eq!(operation.inputs[0].thread_schedule, [0, 1, 2]);
        let input = &operation.inputs[0].input_hex;
        let bytes = input
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        let command = String::from_utf8(bytes).unwrap();
        assert!(command.starts_with("THES:SHELL:operation:"));
        assert!(command.contains("\"command\":[\"/bin/cat\",\"/health\"]"));
        assert!(command.contains("\"expect_exit\":0"));
        assert!(command.contains("\"output_json\":true"));
        assert!(command.contains("\"CHECK_MODE\":\"full\""));
        assert!(command.contains("\"THESEUS_CHOICES\":\"mode=0,retry=0\""));
        assert!(command.contains("\"THESEUS_THREAD_SCHEDULE\":\"0,1,2\""));
    }

    #[test]
    fn expands_a_bounded_thread_schedule_search_into_locked_cases() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [test]\nnetworks:\n  test: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 123\n    max_operations_per_run: 1\n    operations:\n      - name: race\n        shell:\n          command: [/usr/local/bin/race]\n          output_json: true\n          thread_schedule:\n            threads: [0, 1, 2]\n            period: 5\n            max_switches: 3\n    faults: []\n",
        );
        let root = directory.path().join("api");
        fs::write(root.join("runtime/theseus-image"), b"image adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            root.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(root.join("guest/service.tar"), b"image").unwrap();
        fs::write(
            root.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[container_service.ready]\nurl = 'http://127.0.0.1:8080/health'\n",
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let operation = &plan.campaign.as_ref().unwrap().operations[0];
        let search = operation.thread_schedule_search.as_ref().unwrap();
        assert_eq!(search.generated_schedules, 123);
        assert!(operation.thread_schedule.is_empty());
        assert_eq!(operation.inputs.len(), 123);
        assert_eq!(operation.inputs[0].name, "schedule-0-0-0-0-0");
        let lost_update = operation
            .inputs
            .iter()
            .find(|input| input.thread_schedule == [0, 0, 0, 1, 2])
            .unwrap();
        let bytes = lost_update
            .input_hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        let command = String::from_utf8(bytes).unwrap();
        assert!(command.contains("\"THESEUS_THREAD_SCHEDULE\":\"0,0,0,1,2\""));
    }

    #[test]
    fn locks_runnable_prefix_exploration_without_precomputing_schedules() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [test]\nnetworks:\n  test: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 32\n    max_operations_per_run: 1\n    operations:\n      - name: race\n        shell:\n          command: [/usr/local/bin/race]\n          output_json: true\n          thread_schedule:\n            runnable_prefixes:\n              max_choices: 16\n              max_variants: 32\n    faults: []\n",
        );
        let root = directory.path().join("api");
        fs::write(root.join("runtime/theseus-image"), b"image adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            root.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(root.join("guest/service.tar"), b"image").unwrap();
        fs::write(
            root.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[container_service.ready]\nurl = 'http://127.0.0.1:8080/health'\n",
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let operation = &plan.campaign.as_ref().unwrap().operations[0];
        let exploration = operation.thread_schedule_exploration.as_ref().unwrap();
        assert_eq!(exploration.strategy, "runnable_prefixes");
        assert_eq!(exploration.max_choices, 16);
        assert_eq!(exploration.max_variants, 32);
        assert_eq!(operation.inputs.len(), 1);
        let bytes = operation.inputs[0]
            .input_hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        let command = String::from_utf8(bytes).unwrap();
        assert!(command.contains("\"THESEUS_THREAD_SCHEDULE_MODE\":\"runnable_prefix\""));
        assert!(command.contains("\"THESEUS_THREAD_SCHEDULE\":\"\""));
    }

    #[test]
    fn rejects_unbounded_or_ambiguous_thread_schedule_searches() {
        for search in [
            ComposeThreadScheduleSearch {
                threads: vec![0],
                period: 5,
                max_switches: 2,
            },
            ComposeThreadScheduleSearch {
                threads: vec![0, 1, 1],
                period: 5,
                max_switches: 2,
            },
            ComposeThreadScheduleSearch {
                threads: vec![1, 2],
                period: 5,
                max_switches: 2,
            },
            ComposeThreadScheduleSearch {
                threads: vec![0, 32],
                period: 5,
                max_switches: 2,
            },
            ComposeThreadScheduleSearch {
                threads: vec![0, 1],
                period: 17,
                max_switches: 2,
            },
            ComposeThreadScheduleSearch {
                threads: vec![0, 1],
                period: 5,
                max_switches: 6,
            },
            ComposeThreadScheduleSearch {
                threads: vec![0, 1, 2, 3],
                period: 8,
                max_switches: 3,
            },
        ] {
            let error = thread_schedule_search_patterns(&search, "race").unwrap_err();
            assert!(error.to_string().contains("thread schedule search"));
        }
    }

    #[test]
    fn rejects_invalid_or_explosive_structured_choices() {
        for bound in [0, 257] {
            let error = structured_choice_assignments(
                &BTreeMap::from([("mode".to_owned(), bound)]),
                "calculate",
            )
            .unwrap_err();
            assert!(error.to_string().contains("bound between 1 and 256"));
        }
        let error = structured_choice_assignments(
            &BTreeMap::from([
                ("first".to_owned(), 8),
                ("second".to_owned(), 8),
                ("third".to_owned(), 8),
            ]),
            "calculate",
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("more than 256 structured choice assignments"));
    }

    #[test]
    fn thread_schedule_switch_bound_includes_the_period_boundary() {
        let only_constant = thread_schedule_search_patterns(
            &ComposeThreadScheduleSearch {
                threads: vec![0, 1],
                period: 2,
                max_switches: 1,
            },
            "race",
        )
        .unwrap();
        assert_eq!(only_constant, [vec![0, 0], vec![1, 1]]);

        let one_change_and_wrap = thread_schedule_search_patterns(
            &ComposeThreadScheduleSearch {
                threads: vec![0, 1],
                period: 2,
                max_switches: 2,
            },
            "race",
        )
        .unwrap();
        assert_eq!(
            one_change_and_wrap,
            [vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1]]
        );
    }

    #[test]
    fn locks_named_shell_process_lifecycle_for_overlapping_commands() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 4\n    max_operations_per_run: 6\n    operations:\n      - name: launch_writer\n        shell:\n          phase: launch\n          process: writer-a\n          command: [/bin/writer, a]\n      - name: complete_writer\n        shell:\n          phase: completion\n          process: writer-a\n          output_contains: committed\n    faults: []\n",
        );
        let root = directory.path().join("api");
        fs::write(root.join("runtime/theseus-image"), b"image adapter").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            root.join("runtime/theseus-image"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(root.join("guest/service.tar"), b"image").unwrap();
        fs::write(
            root.join("theseus.toml"),
            "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n[container_service.ready]\nurl = 'http://127.0.0.1:8080/health'\n",
        )
        .unwrap();

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let operations = &plan.campaign.unwrap().operations;
        assert_eq!(operations[0].shell_phase, Some(ComposeShellPhase::Launch));
        assert_eq!(operations[0].shell_process.as_deref(), Some("writer-a"));
        assert_eq!(
            operations[1].shell_phase,
            Some(ComposeShellPhase::Completion)
        );
        let completion = operations[1].inputs[0]
            .input_hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        let completion = String::from_utf8(completion).unwrap();
        assert!(completion.contains("\"phase\":\"completion\""));
        assert!(completion.contains("\"command\":[]"));
        assert!(completion.contains("\"output_contains\":\"committed\""));
    }

    #[test]
    fn locks_a_complete_test_command_lifecycle() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [test]\nnetworks:\n  test: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_operations_per_run: 7\n    operations:\n      - {name: prepare, command: first, input: 'prepare\\n'}\n      - {name: write, command: parallel_driver, input: 'write\\n'}\n      - {name: compact, command: serial_driver, input: 'compact\\n'}\n      - {name: legacy, command: singleton_driver, input: 'legacy\\n'}\n      - {name: inspect, command: anytime, input: 'inspect\\n'}\n      - {name: recover, command: eventually, input: 'recover\\n'}\n      - {name: verify, command: finally, input: 'verify\\n'}\n    faults: []\n",
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let commands = plan
            .campaign
            .unwrap()
            .operations
            .into_iter()
            .map(|operation| operation.command.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            commands,
            [
                ComposeTestCommand::First,
                ComposeTestCommand::ParallelDriver,
                ComposeTestCommand::SerialDriver,
                ComposeTestCommand::SingletonDriver,
                ComposeTestCommand::Anytime,
                ComposeTestCommand::Eventually,
                ComposeTestCommand::Finally,
            ]
        );
    }

    #[test]
    fn rejects_partial_or_stage_mixed_test_command_models() {
        for extra in [
            "operations:\n      - {name: prepare, command: first, input: 'prepare\\n'}\n      - {name: write, input: 'write\\n'}",
            "stages: [work]\n    operations:\n      - {name: write, command: serial_driver, stage: work, input: 'write\\n'}",
        ] {
            let directory = fixture(&format!(
                "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [test]\nnetworks:\n  test: {{}}\nx-theseus:\n  campaign:\n    driver: api\n    {extra}\n    faults: []\n"
            ));
            let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
            assert!(error.to_string().contains("test-command"));
        }
    }

    #[test]
    fn rejects_campaign_actions_after_quiet_lifecycle_commands() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [test]\nnetworks:\n  test: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - {name: prepare, command: first, input: 'prepare\\n'}\n      - {name: write, command: serial_driver, input: 'write\\n'}\n    faults:\n      - {kind: partition, network: test, after: prepare}\n",
        );

        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("lifecycle-protected test command"));
    }

    #[test]
    fn rejects_malformed_shell_process_lifecycle() {
        for shell in [
            "phase: launch\n          command: [/bin/writer]",
            "phase: completion\n          process: writer\n          command: [/bin/wait]",
            "phase: run\n          process: writer\n          command: [/bin/writer]",
            "phase: launch\n          process: writer\n          command: [/bin/writer]\n          output_json: true",
            "command: [/bin/writer]\n          thread_schedule: [32]",
            "command: [/bin/writer]\n          environment: {THESEUS_THREAD_SCHEDULE: '0,1'}",
            "phase: completion\n          process: writer\n          thread_schedule: [0]",
        ] {
            let directory = fixture(&format!(
                "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [test]\nnetworks:\n  test: {{}}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: command\n        shell:\n          {shell}\n    faults: []\n"
            ));
            let root = directory.path().join("api");
            fs::write(root.join("runtime/theseus-image"), b"image adapter").unwrap();
            #[cfg(unix)]
            fs::set_permissions(
                root.join("runtime/theseus-image"),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            fs::write(root.join("guest/service.tar"), b"image").unwrap();
            fs::write(
                root.join("theseus.toml"),
                "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\nimage_adapter = 'runtime/theseus-image'\n[guest]\nkernel = 'guest/vmlinux'\nimage = 'guest/service.tar'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1\nexits_per_tick = 1\n[container_service.ready]\nurl = 'http://127.0.0.1:8080'\n",
            )
            .unwrap();
            let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
            assert!(
                error.to_string().contains("invalid shell contract")
                    || error.to_string().contains("invalid thread schedule"),
                "unexpected error for {shell:?}: {error}"
            );
        }
    }

    #[test]
    fn rejects_host_compose_features() {
        let directory = fixture(
            "services:\n  api:\n    image: nginx\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("unknown field `image`"));
    }

    #[test]
    fn rejects_undeclared_network() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [missing]\nnetworks:\n  backplane: {}\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("undeclared network"));
    }

    #[test]
    fn locks_per_service_lifecycle_and_clock_schedule() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      faults:\n        - at_round: 2\n          kind: pause\n          duration_rounds: 3\n        - at_round: 6\n          kind: restart\n        - at_round: 8\n          kind: clock_jump\n          nanoseconds: 1000000000\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.services["api"].faults.len(), 3);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["services"]["api"]["faults"][2]["kind"], "clock_jump");
    }

    #[test]
    fn accepts_backward_clock_jumps_and_rejects_the_rest() {
        let base = r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
      faults:
        - at_round: 2
          kind: clock_jump
          nanoseconds: __NS__
    networks: [backplane]
networks:
  backplane: {}
"#;
        let directory = fixture(&base.replace("__NS__", "-500000000"));
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.services["api"].faults[0].nanoseconds,
            Some(-500_000_000)
        );

        let zero = fixture(&base.replace("__NS__", "0"));
        let error = load_compose_plan(zero.path().join("compose.yaml")).unwrap_err();
        assert!(
            error.to_string().contains("non-zero nanoseconds"),
            "{error}"
        );

        let huge = fixture(&base.replace("__NS__", "7200000000000"));
        let error = load_compose_plan(huge.path().join("compose.yaml")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("magnitude must be at most 3600000000000"),
            "{error}"
        );
    }

    #[test]
    fn rejects_clock_jump_without_virtual_time() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      faults:\n        - at_round: 1\n          kind: clock_jump\n          nanoseconds: 1\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let manifest = directory.path().join("api/theseus.toml");
        let input = fs::read_to_string(&manifest).unwrap();
        fs::write(
            manifest,
            input.replace(
                "[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
                "",
            ),
        )
        .unwrap();
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("clock_jump requires virtual_time"));
    }

    #[test]
    fn normalizes_a_serial_driven_topology_campaign() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 8\n    operations:\n      - name: put\n        input: \"put alpha\\n\"\n      - name: get\n        input: \"get alpha\\n\"\n        requires_serial:\n          service: worker\n          json:\n            fields:\n              /event: ready\n        requires_serial_all:\n          - contains: THES:M:ready\n          - service: worker\n            json:\n              fields:\n                /role: replica\n        requires_serial_joins:\n          - endpoints:\n              - pointer: /request_id\n                json:\n                  fields:\n                    /event: write\n              - service: worker\n                pointer: /request_id\n                json:\n                  fields:\n                    /event: replicated\n        excludes_serial_joins:\n          - endpoints:\n              - pointer: /request_id\n                json:\n                  fields:\n                    /event: completed\n              - service: worker\n                pointer: /request_id\n                json:\n                  fields:\n                    /event: committed\n    faults:\n      - service: worker\n        at_round: 2\n        kind: restart\n    properties:\n      - name: no_data_loss\n        kind: always\n        service: api\n        contains: 'THES:ASSERT:no_data_loss:pass'\n      - name: stale_read_is_reachable\n        kind: reachable\n        contains: 'THES:ASSERT:stale_read:fail'\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.expect("campaign is normalized");
        assert_eq!(campaign.driver, "api");
        assert_eq!(
            campaign.operations[0].inputs[0].input_hex,
            "70757420616c7068610a"
        );
        assert_eq!(campaign.faults[0].service.as_deref(), Some("worker"));
        assert_eq!(campaign.max_faults_per_run, 2);
        assert_eq!(campaign.max_operations_per_run, 3);
        assert_eq!(
            campaign.operations[1]
                .requires_serial
                .as_ref()
                .unwrap()
                .service
                .as_deref(),
            Some("worker")
        );
        assert_eq!(campaign.operations[1].requires_serial_all.len(), 2);
        assert_eq!(
            campaign.operations[1].requires_serial_joins[0]
                .endpoints
                .len(),
            2
        );
        assert_eq!(
            campaign.operations[1].excludes_serial_joins[0]
                .endpoints
                .len(),
            2
        );
        assert_eq!(
            campaign.operations[1].requires_serial_all[1]
                .service
                .as_deref(),
            Some("worker")
        );
        assert_eq!(campaign.properties.len(), 2);

        let compose = directory.path().join("compose.yaml");
        let input = fs::read_to_string(&compose).unwrap();
        fs::write(
            &compose,
            input.replace(
                "contains: 'THES:ASSERT:no_data_loss:pass'",
                "requires_serial_all:\n          - service: worker\n            json:\n              fields:\n                /event: ready",
            ),
        )
        .unwrap();
        let joined = load_compose_plan(&compose).unwrap();
        assert_eq!(
            joined.campaign.unwrap().properties[0]
                .requires_serial_all
                .len(),
            1
        );
        fs::write(
            &compose,
            input.replace(
                "service: worker\n          json",
                "service: missing\n          json",
            ),
        )
        .unwrap();
        let error = load_compose_plan(&compose).unwrap_err();
        assert!(error
            .to_string()
            .contains("unknown serial-guard service \"missing\""));
    }

    #[test]
    fn normalizes_adaptive_campaign_guidance() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    guidance: adaptive\n    max_runs: 1\n    operations:\n      - name: probe\n        input: \"probe\\n\"\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.campaign.expect("campaign is normalized").guidance,
            CampaignGuidance::Adaptive
        );
    }

    #[test]
    fn normalizes_application_block_campaign_coverage() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    coverage: application_blocks\n    max_runs: 1\n    operations:\n      - name: probe\n        input: \"probe\\n\"\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.campaign.expect("campaign is normalized").coverage,
            CampaignCoverage::ApplicationBlocks
        );
    }

    #[test]
    fn normalizes_application_edge_campaign_coverage() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    coverage: application_edges\n    max_runs: 1\n    operations:\n      - name: probe\n        input: \"probe\\n\"\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.campaign.expect("campaign is normalized").coverage,
            CampaignCoverage::ApplicationEdges
        );
    }

    #[test]
    fn normalizes_posterior_campaign_guidance() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    guidance: posterior\n    max_runs: 1\n    operations:\n      - name: probe\n        input: \"probe\\n\"\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.campaign.expect("campaign is normalized").guidance,
            CampaignGuidance::Posterior
        );
    }

    #[test]
    fn normalizes_property_directed_campaign_guidance() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    guidance: property\n    max_runs: 1\n    operations:\n      - name: probe\n        input: \"probe\\n\"\n    properties:\n      - name: probe_is_reachable\n        kind: reachable\n        contains: THES:M:probe\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.campaign.expect("campaign is normalized").guidance,
            CampaignGuidance::Property
        );
    }

    #[test]
    fn normalizes_a_campaign_operation_for_another_service() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: compact\n        service: worker\n        input: \"compact\\n\"\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(
            plan.campaign.expect("campaign is normalized").operations[0].service,
            "worker"
        );
    }

    #[test]
    fn rejects_a_campaign_operation_for_an_unknown_service() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: compact\n        service: missing\n        input: \"compact\\n\"\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("targets unknown service \"missing\""));
    }

    #[test]
    fn parses_compound_campaign_property_predicates() {
        let property: ComposeProperty = serde_yaml::from_str(
            "name: durable_write\nkind: always\ncontains: THES:ASSERT:write:pass\ncontains_all: [THES:M:written]\ncontains_any: [THES:CHECKPOINT:write, THES:M:written]\ncontains_none: [THES:ASSERT:panic]\n",
        )
        .unwrap();

        assert_eq!(property.contains_all, ["THES:M:written"]);
        assert_eq!(
            property.contains_any,
            ["THES:CHECKPOINT:write", "THES:M:written"]
        );
        assert_eq!(property.contains_none, ["THES:ASSERT:panic"]);
    }

    #[test]
    fn normalizes_cross_service_json_correlations() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: write\n        input: \"write\\n\"\n    faults: []\n    properties:\n      - name: replicated_write\n        kind: always\n        service: api\n        contains: THES:ASSERT:replicated_write:pass\n        requires_serial_correlations:\n          - capture:\n              pointer: /request_id\n              json:\n                fields:\n                  /event: write\n            equals:\n              service: worker\n              pointer: /request_id\n              json:\n                fields:\n                  /event: replicated\n        requires_serial_joins:\n          - endpoints:\n              - pointers: [/request_id, /attempt]\n                json:\n                  fields:\n                    /event: write\n              - service: worker\n                pointers: [/request_id, /attempt]\n                json:\n                  fields:\n                    /event: replicated\n",
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.unwrap();
        let property = &campaign.properties[0];
        let correlation = &property.requires_serial_correlations[0];
        assert_eq!(correlation.capture.service, None);
        assert_eq!(correlation.capture.pointers, ["/request_id"]);
        assert_eq!(correlation.equals.service.as_deref(), Some("worker"));
        assert_eq!(
            correlation.equals.json.fields["/event"],
            serde_json::Value::String("replicated".to_owned())
        );
        assert_eq!(property.requires_serial_joins[0].endpoints.len(), 2);
        assert_eq!(
            property.requires_serial_joins[0].endpoints[0].pointers,
            ["/request_id", "/attempt"]
        );
    }

    #[test]
    fn rejects_cross_service_json_correlation_with_unknown_service() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: write\n        input: \"write\\n\"\n    faults: []\n    properties:\n      - name: replicated_write\n        kind: always\n        service: api\n        requires_serial_correlations:\n          - capture:\n              pointer: /request_id\n              json:\n                fields:\n                  /event: write\n            equals:\n              service: missing\n              pointer: /request_id\n              json:\n                fields:\n                  /event: replicated\n",
        );

        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("correlation references unknown service \"missing\""));
    }

    #[test]
    fn rejects_single_endpoint_json_joins() {
        let join: ComposeSerialJoin = serde_yaml::from_str(
            "endpoints:\n  - pointer: /request_id\n    json:\n      fields:\n        /event: write\n",
        )
        .unwrap();

        let error = normalize_serial_joins(Some(vec![join]), "replicated_write", &BTreeMap::new())
            .unwrap_err();
        assert!(error.to_string().contains("needs at least two endpoints"));
    }

    #[test]
    fn normalizes_universal_json_joins() {
        let join: ComposeSerialJoin = serde_yaml::from_str(
            "quantifier: every\noccurs:\n  at_least: 1\n  at_most: 2\nendpoints:\n  - pointer: /request_id\n    json:\n      fields:\n        /event: write\n  - pointer: /request_id\n    json:\n      fields:\n        /event: replicated\n",
        )
        .unwrap();

        let plan =
            normalize_serial_joins(Some(vec![join]), "replicated_write", &BTreeMap::new()).unwrap();
        assert_eq!(plan[0].quantifier, ComposeSerialJoinQuantifier::Every);
        assert_eq!(plan[0].occurs.as_ref().unwrap().at_least, Some(1));
        assert_eq!(plan[0].occurs.as_ref().unwrap().at_most, Some(2));
    }

    #[test]
    fn rejects_invalid_json_join_match_counts() {
        let join: ComposeSerialJoin = serde_yaml::from_str(
            "occurs:\n  exactly: 1\n  at_least: 1\nendpoints:\n  - pointer: /request_id\n    json:\n      fields:\n        /event: write\n  - pointer: /request_id\n    json:\n      fields:\n        /event: replicated\n",
        )
        .unwrap();

        let error = normalize_serial_joins(Some(vec![join]), "replicated_write", &BTreeMap::new())
            .unwrap_err();
        assert!(error.to_string().contains("match count needs exactly"));
    }

    #[test]
    fn rejects_ambiguous_composite_json_join_keys() {
        let endpoint: ComposeJsonCorrelationEndpoint = serde_yaml::from_str(
            "pointer: /request_id\npointers: [/request_id, /attempt]\njson:\n  fields:\n    /event: write\n",
        )
        .unwrap();

        let error = normalize_json_correlation_endpoint(
            endpoint,
            "join",
            "replicated_write",
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pointer or pointers, not both"));
    }

    #[test]
    fn normalizes_recursive_serial_evidence() {
        let evidence: ComposeSerialEvidence = serde_yaml::from_str(
            "all:\n  - guard:\n      json:\n        fields:\n          /event: replicated\n  - any:\n      - correlation:\n          capture:\n            pointer: /request_id\n            json:\n              fields:\n                /event: write\n          equals:\n            pointer: /request_id\n            json:\n              fields:\n                /event: replicated\n      - join:\n          endpoints:\n            - pointers: [/request_id, /attempt]\n              json:\n                fields:\n                  /event: write\n            - pointers: [/request_id, /attempt]\n              json:\n                fields:\n                  /event: replicated\n  - relation:\n      left:\n        pointer: /attempt\n        json:\n          fields:\n            /event: replicated\n      right:\n        pointer: /attempt\n        json:\n          fields:\n            /event: write\n      operator: greater_than_or_equal\n",
        )
        .unwrap();
        let plan = normalize_serial_evidence_root(
            evidence,
            "write",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &mut BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(plan.all.len(), 3);
        assert_eq!(plan.all[1].any.len(), 2);
        assert!(plan.all[1].any[0].correlation.is_some());
        assert!(plan.all[1].any[1].join.is_some());
        assert_eq!(
            plan.all[2].relation.as_ref().unwrap().operator,
            ComposeJsonRelationOperator::GreaterThanOrEqual
        );
    }

    #[test]
    fn rejects_ambiguous_serial_evidence_expressions() {
        let evidence: ComposeSerialEvidence = serde_yaml::from_str(
            "all:\n  - guard:\n      contains: THES:M:written\nguard:\n  contains: THES:M:written\n",
        )
        .unwrap();

        let error = normalize_serial_evidence_root(
            evidence,
            "write",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &mut BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exactly one of all, any, none"));
    }

    #[test]
    fn rejects_a_composite_numeric_serial_relation() {
        let relation: ComposeSerialRelation = serde_yaml::from_str(
            "left:\n  pointers: [/attempt, /generation]\n  json:\n    fields:\n      /event: replicated\nright:\n  pointers: [/attempt, /generation]\n  json:\n    fields:\n      /event: write\noperator: greater_than\n",
        )
        .unwrap();

        let error = normalize_serial_relation(relation, "write", &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("numeric relation needs one"));
    }

    #[test]
    fn normalizes_quantified_json_relations() {
        let relation: ComposeSerialRelation = serde_yaml::from_str(
            "order: after\nquantifier: every\noccurs:\n  exactly: 1\nleft:\n  pointer: /attempt\n  json:\n    fields:\n      /event: replicated\nright:\n  pointer: /attempt\n  json:\n    fields:\n      /event: write\noperator: greater_than_or_equal\n",
        )
        .unwrap();

        let plan = normalize_serial_relation(relation, "write", &BTreeMap::new()).unwrap();
        assert_eq!(plan.order, Some(ComposeSerialRelationOrder::After));
        assert_eq!(plan.quantifier, ComposeSerialJoinQuantifier::Every);
        assert_eq!(plan.occurs.unwrap().exactly, Some(1));
    }

    #[test]
    fn rejects_ordered_json_relations_across_services() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: write\n        input: \"write\\n\"\n    faults: []\n    properties:\n      - name: ordered_write\n        kind: always\n        requires_serial_evidence:\n          relation:\n            order: before\n            left:\n              service: api\n              pointer: /request_id\n              json:\n                fields:\n                  /event: write\n            right:\n              service: worker\n              pointer: /request_id\n              json:\n                fields:\n                  /event: replicated\n            operator: equals\n",
        );

        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("same left and right service"));
    }

    #[test]
    fn normalizes_keyed_json_event_paths() {
        let path: ComposeSerialPath = serde_yaml::from_str(
            "pointers: [/request_id, /attempt]\nquantifier: every\noccurs:\n  exactly: 1\nsteps:\n  - fields:\n      /event: write\n  - fields:\n      /event: replicated\n  - fields:\n      /event: committed\n",
        )
        .unwrap();

        let plan = normalize_serial_path(path, "write", &BTreeMap::new()).unwrap();
        assert_eq!(plan.pointers, ["/request_id", "/attempt"]);
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.quantifier, ComposeSerialJoinQuantifier::Every);
        assert_eq!(plan.occurs.unwrap().exactly, Some(1));
    }

    #[test]
    fn rejects_short_json_event_paths() {
        let path: ComposeSerialPath = serde_yaml::from_str(
            "pointers: [/request_id, /request_id]\nsteps:\n  - fields:\n      /event: write\n",
        )
        .unwrap();

        let error = normalize_serial_path(path, "write", &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("at least two JSON event steps"));
    }

    #[test]
    fn normalizes_cross_service_json_workflows() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    operations:\n      - name: write\n        input: \"write\\n\"\n    faults: []\n    properties:\n      - name: replicated_write\n        kind: always\n        requires_serial_evidence:\n          workflow:\n            pointers: [/request_id]\n            quantifier: every\n            occurs:\n              exactly: 1\n            stages:\n              - service: api\n                steps:\n                  - fields:\n                      /event: write\n                  - fields:\n                      /event: accepted\n              - service: worker\n                pointers: [/source_request_id]\n                steps:\n                  - fields:\n                      /event: replicated\n",
        )
        ;
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.unwrap();
        let plan = campaign.properties[0]
            .requires_serial_evidence
            .as_ref()
            .unwrap()
            .workflow
            .as_ref()
            .unwrap();
        assert_eq!(plan.stages.len(), 2);
        assert_eq!(plan.stages[0].service, "api");
        assert_eq!(plan.stages[0].steps.len(), 2);
        assert_eq!(plan.stages[1].pointers, ["/source_request_id"]);
        assert_eq!(plan.occurs.as_ref().unwrap().exactly, Some(1));
    }

    #[test]
    fn resolves_named_serial_evidence_across_campaign_rules() {
        let definitions: BTreeMap<String, ComposeSerialEvidence> = serde_yaml::from_str(
            "write_seen:\n  guard:\n    json:\n      fields:\n        /event: write\nreplicated_write:\n  all:\n    - use: write_seen\n    - join:\n        endpoints:\n          - pointers: [/request_id, /attempt]\n            json:\n              fields:\n                /event: write\n          - pointers: [/request_id, /attempt]\n            json:\n              fields:\n                /event: replicated\nready_for_retry:\n  all:\n    - use: replicated_write\n    - none:\n        - guard:\n            contains: THES:ASSERT:panic\n",
        )
        .unwrap();
        let mut resolved =
            normalize_serial_evidence_definitions(&definitions, &BTreeMap::new()).unwrap();
        let evidence: ComposeSerialEvidence =
            serde_yaml::from_str("use: ready_for_retry\n").unwrap();

        let plan = normalize_serial_evidence_root(
            evidence,
            "retry",
            &BTreeMap::new(),
            &definitions,
            &mut resolved,
        )
        .unwrap();
        assert_eq!(plan.all.len(), 2);
        assert_eq!(plan.all[0].all.len(), 2);
        assert_eq!(plan.all[0].all[1].join.as_ref().unwrap().endpoints.len(), 2);
        assert_eq!(plan.all[1].none.len(), 1);
    }

    #[test]
    fn rejects_unknown_or_cyclic_named_serial_evidence() {
        let definitions: BTreeMap<String, ComposeSerialEvidence> =
            serde_yaml::from_str("first:\n  use: second\nsecond:\n  use: first\n").unwrap();
        let error =
            normalize_serial_evidence_definitions(&definitions, &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("is cyclic"));

        let evidence: ComposeSerialEvidence = serde_yaml::from_str("use: missing\n").unwrap();
        let error = normalize_serial_evidence_root(
            evidence,
            "retry",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &mut BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("unknown serial evidence \"missing\""));
    }

    #[test]
    fn expands_named_serial_evidence_for_operations_and_properties() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 1\n    evidence:\n      write_seen:\n        guard:\n          json:\n            fields:\n              /event: write\n      safe_write:\n        all:\n          - use: write_seen\n          - none:\n              - guard:\n                  contains: THES:ASSERT:panic\n    operations:\n      - name: retry\n        input: \"retry\\n\"\n        requires_serial_evidence:\n          use: safe_write\n    faults: []\n    properties:\n      - name: safe_write\n        kind: always\n        requires_serial_evidence:\n          use: safe_write\n        excludes_serial_evidence:\n          guard:\n            contains: THES:ASSERT:panic\n",
        );

        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert_eq!(
            campaign.operations[0]
                .requires_serial_evidence
                .as_ref()
                .unwrap()
                .all
                .len(),
            2
        );
        assert_eq!(
            campaign.properties[0]
                .requires_serial_evidence
                .as_ref()
                .unwrap()
                .all
                .len(),
            2
        );
        assert!(campaign.properties[0].excludes_serial_evidence.is_some());
    }

    #[test]
    fn parses_nested_campaign_property_predicates() {
        let property: ComposeProperty = serde_yaml::from_str(
            "name: durable_write\nkind: always\npredicate:\n  all:\n    - contains: THES:ASSERT:write:pass\n    - any:\n        - matches: THES:CHECKPOINT:write_[0-9]+\n        - json:\n            fields:\n              /event: checkpoint\n            where:\n              - pointer: /operation\n                matches: '^write_[0-9]+$'\n              - pointer: /attempt\n                greater_than_or_equal: 2\n    - none:\n        - contains: THES:ASSERT:panic\n",
        )
        .unwrap();
        let predicate =
            normalize_serial_predicate(property.predicate.unwrap(), &property.name).unwrap();

        assert_eq!(predicate.all.len(), 3);
        assert_eq!(predicate.all[1].any.len(), 2);
        assert_eq!(
            predicate.all[1].any[0].matches.as_deref(),
            Some("THES:CHECKPOINT:write_[0-9]+")
        );
        assert_eq!(
            predicate.all[1].any[1].json.as_ref().unwrap().where_[0]
                .matches
                .as_deref(),
            Some("^write_[0-9]+$")
        );
        assert_eq!(
            predicate.all[1].any[1].json.as_ref().unwrap().where_[1].greater_than_or_equal,
            Some(2.0)
        );
        assert_eq!(predicate.all[2].none.len(), 1);
    }

    #[test]
    fn rejects_invalid_nested_campaign_regexes() {
        let error = normalize_serial_predicate(
            ComposeSerialPredicate {
                contains: None,
                matches: Some("[".to_owned()),
                json: None,
                all: Vec::new(),
                any: Vec::new(),
                none: Vec::new(),
                sequence: Vec::new(),
                occurs: None,
            },
            "durable_write",
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid nested regex"));
    }

    #[test]
    fn rejects_invalid_nested_campaign_json_pointers() {
        let error = normalize_serial_predicate(
            ComposeSerialPredicate {
                contains: None,
                matches: None,
                json: Some(ComposeJsonPredicate {
                    query: None,
                    fields: BTreeMap::from([(
                        "event".to_owned(),
                        serde_json::Value::String("checkpoint".to_owned()),
                    )]),
                    where_: Vec::new(),
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
            "durable_write",
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid nested JSON pointer"));
    }

    #[test]
    fn rejects_ambiguous_nested_campaign_json_conditions() {
        let error = normalize_serial_predicate(
            ComposeSerialPredicate {
                contains: None,
                matches: None,
                json: Some(ComposeJsonPredicate {
                    query: None,
                    fields: BTreeMap::new(),
                    where_: vec![ComposeJsonCondition {
                        pointer: "/attempt".to_owned(),
                        equals: None,
                        matches: None,
                        greater_than: Some(1.0),
                        greater_than_or_equal: Some(2.0),
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
            "durable_write",
        )
        .unwrap_err();

        assert!(error.to_string().contains("exactly one operator"));
    }

    #[test]
    fn normalizes_nested_campaign_json_array_predicates() {
        let predicate: ComposeSerialPredicate = serde_yaml::from_str(
            "json:\n  fields:\n    /event: ready\n  arrays:\n    - pointer: /checks\n      all:\n        where:\n          - pointer: /passed\n            equals: true\n    - pointer: /checks\n      any:\n        fields:\n          /name: serial\n",
        )
        .unwrap();

        let predicate = normalize_serial_predicate(predicate, "ready").unwrap();
        assert_eq!(predicate.json.as_ref().unwrap().arrays.len(), 2);
        assert!(predicate.json.as_ref().unwrap().arrays[0].all.is_some());
        assert!(predicate.json.as_ref().unwrap().arrays[1].any.is_some());
    }

    #[test]
    fn normalizes_rfc9535_jsonpath_event_queries() {
        let predicate: ComposeSerialPredicate = serde_yaml::from_str(
            r#"json:
  query: '$.checks[?@.name == "serial" && @.passed == true]'
"#,
        )
        .unwrap();
        let predicate = normalize_serial_predicate(predicate, "auditor_ready").unwrap();
        assert_eq!(
            predicate.json.as_ref().unwrap().query.as_deref(),
            Some("$.checks[?@.name == \"serial\" && @.passed == true]")
        );

        let invalid: ComposeSerialPredicate =
            serde_yaml::from_str("json:\n  query: '$.checks['\n").unwrap();
        let error = normalize_serial_predicate(invalid, "auditor_ready").unwrap_err();
        assert!(error.to_string().contains("invalid JSONPath query"));
    }

    #[test]
    fn normalizes_ordered_serial_predicates() {
        let predicate: ComposeSerialPredicate = serde_yaml::from_str(
            "sequence:\n  - contains: booted\n  - matches: 'THES:CHECKPOINT:write'\n  - json:\n      fields:\n        /event: assertion\n        /passed: false\n",
        )
        .unwrap();

        let predicate = normalize_serial_predicate(predicate, "stale_read").unwrap();
        assert_eq!(predicate.sequence.len(), 3);
        assert_eq!(predicate.sequence[0].contains.as_deref(), Some("booted"));
        assert_eq!(
            predicate.sequence[1].matches.as_deref(),
            Some("THES:CHECKPOINT:write")
        );
        assert_eq!(
            predicate.sequence[2].json.as_ref().unwrap().fields["/event"],
            serde_json::Value::String("assertion".to_owned())
        );
    }

    #[test]
    fn normalizes_ordered_json_capture_predicates() {
        let predicate: ComposeSerialPredicate = serde_yaml::from_str(
            "sequence:\n  - json:\n      fields:\n        /event: started\n      capture:\n        request: /request_id\n  - json:\n      fields:\n        /event: completed\n      equals_capture:\n        /request_id: request\n",
        )
        .unwrap();

        let predicate = normalize_serial_predicate(predicate, "request_completed").unwrap();
        assert_eq!(
            predicate.sequence[0].json.as_ref().unwrap().capture["request"],
            "/request_id"
        );
        assert_eq!(
            predicate.sequence[1].json.as_ref().unwrap().equals_capture["/request_id"],
            "request"
        );
    }

    #[test]
    fn rejects_json_capture_outside_an_ordered_sequence() {
        let predicate: ComposeSerialPredicate =
            serde_yaml::from_str("json:\n  capture:\n    request: /request_id\n").unwrap();

        let error = normalize_serial_predicate(predicate, "request_completed").unwrap_err();
        assert!(error
            .to_string()
            .contains("JSON captures are only allowed in sequence items"));
    }

    #[test]
    fn rejects_json_capture_inside_a_boolean_branch() {
        let predicate: ComposeSerialPredicate = serde_yaml::from_str(
            "sequence:\n  - json:\n      all:\n        - capture:\n            request: /request_id\n",
        )
        .unwrap();

        let error = normalize_serial_predicate(predicate, "request_completed").unwrap_err();
        assert!(error
            .to_string()
            .contains("JSON captures are only allowed in sequence items"));
    }

    #[test]
    fn rejects_unknown_ordered_json_capture() {
        let predicate: ComposeSerialPredicate = serde_yaml::from_str(
            "sequence:\n  - json:\n      fields:\n        /event: completed\n      equals_capture:\n        /request_id: request\n",
        )
        .unwrap();

        let error = normalize_serial_predicate(predicate, "request_completed").unwrap_err();
        assert!(error
            .to_string()
            .contains("must be declared by an earlier sequence item"));
    }

    #[test]
    fn rejects_ambiguous_ordered_serial_predicates() {
        let predicate: ComposeSerialPredicate =
            serde_yaml::from_str("sequence:\n  - contains: booted\n    matches: ready\n").unwrap();

        let error = normalize_serial_predicate(predicate, "stale_read").unwrap_err();
        assert!(error
            .to_string()
            .contains("sequence items must contain exactly one"));
    }

    #[test]
    fn normalizes_counted_serial_predicates() {
        let predicate: ComposeSerialPredicate = serde_yaml::from_str(
            "occurs:\n  exactly: 2\n  predicate:\n    json:\n      fields:\n        /event: retry\n",
        )
        .unwrap();

        let predicate = normalize_serial_predicate(predicate, "retried").unwrap();
        let occurs = predicate.occurs.unwrap();
        assert_eq!(occurs.exactly, Some(2));
        assert_eq!(
            occurs.predicate.json.as_ref().unwrap().fields["/event"],
            serde_json::Value::String("retry".to_owned())
        );
    }

    #[test]
    fn rejects_invalid_counted_serial_predicate_bounds() {
        let predicate: ComposeSerialPredicate = serde_yaml::from_str(
            "occurs:\n  exactly: 2\n  at_least: 1\n  predicate:\n    contains: retry\n",
        )
        .unwrap();

        let error = normalize_serial_predicate(predicate, "retried").unwrap_err();
        assert!(error.to_string().contains("occurs needs exactly"));
    }

    #[test]
    fn normalizes_campaign_operation_requirements() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
      - name: read
        input: "read\n"
        requires: [write]
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert_eq!(campaign.operations[1].requires, vec!["write"]);
    }

    #[test]
    fn normalizes_named_campaign_operation_inputs() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    state: {phase: idle}
    operations:
      - name: write
        inputs:
          - name: alpha
            input: "write alpha\n"
            requires_state: {phase: idle}
            sets_state: {phase: written}
          - name: beta
            input: "write beta\n"
            requires: ["write[alpha]"]
            requires_state: {phase: written}
            max_uses: 1
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert_eq!(campaign.operations[0].inputs.len(), 2);
        assert_eq!(campaign.operations[0].inputs[0].name, "alpha");
        assert_eq!(
            campaign.operations[0].inputs[0].input_hex,
            "777269746520616c7068610a"
        );
        assert_eq!(campaign.operations[0].inputs[1].name, "beta");
        assert_eq!(
            campaign.operations[0].inputs[1].requires[0],
            OperationInputReferencePlan {
                operation: "write".to_owned(),
                input: Some("alpha".to_owned()),
            }
        );
        assert_eq!(campaign.operations[0].inputs[1].max_uses, Some(1));
        assert_eq!(campaign.state["phase"], "idle");
        assert_eq!(
            campaign.operations[0].inputs[0].sets_state["phase"],
            "written"
        );
        assert_eq!(
            campaign.operations[0].inputs[1].requires_state["phase"],
            "written"
        );
    }

    #[test]
    fn expands_campaign_operation_input_grammar_into_locked_cases() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    state: {phase: fresh}
    operations:
      - name: write
        input_grammar:
          template: "write {value} {mode}\n"
          name_template: "{value}-{mode}"
          choices:
            value: {alpha: alpha, beta: beta}
            mode: {fast: fast, safe: safe}
          cases:
            beta-safe:
              requires: ["write[alpha-fast]"]
              sets_state: {phase: written}
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        let operation = &campaign.operations[0];
        assert_eq!(
            operation
                .inputs
                .iter()
                .map(|input| input.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha-fast", "alpha-safe", "beta-fast", "beta-safe"]
        );
        assert_eq!(
            operation.inputs[0].input_hex,
            "777269746520616c70686120666173740a"
        );
        assert_eq!(
            operation.inputs[3].input_hex,
            "7772697465206265746120736166650a"
        );
        assert_eq!(
            operation.inputs[3].requires[0],
            OperationInputReferencePlan {
                operation: "write".to_owned(),
                input: Some("alpha-fast".to_owned()),
            }
        );
        assert_eq!(operation.inputs[3].sets_state["phase"], "written");
        assert_eq!(
            operation.input_grammar.as_ref().unwrap().template,
            "write {value} {mode}\n"
        );
    }

    #[test]
    fn normalizes_campaign_input_template_captures() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
      - name: retry
        input_template: "retry {request}\n"
        input_captures:
          request:
            pointer: /request_id
            json:
              all:
                - fields:
                    /event: write
                - where:
                    - pointer: /attempt
                      greater_than_or_equal: 1
        requires: [write]
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        let input = &campaign.operations[1].inputs[0];
        assert_eq!(input.input_template.as_deref(), Some("retry {request}\n"));
        assert_eq!(input.input_captures["request"].pointer, "/request_id");
        assert_eq!(
            input.input_captures["request"].select,
            ComposeOperationInputSelect::Latest
        );
        assert_eq!(
            input.input_captures["request"].json.as_ref().unwrap().all[0].fields["/event"],
            serde_json::Value::String("write".to_owned())
        );
        assert_eq!(
            input.input_captures["request"].json.as_ref().unwrap().all[1].where_[0]
                .greater_than_or_equal,
            Some(1.0)
        );
    }

    #[test]
    fn normalizes_campaign_sequenced_input_captures() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: retry
        input_template: "retry {request}\\n"
        input_captures:
          request:
            pointer: /request_id
            sequence:
              - json:
                  fields:
                    /event: started
                  capture:
                    request: /request_id
              - json:
                  fields:
                    /event: write
                  equals_capture:
                    /request_id: request
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        let capture = &campaign.operations[0].inputs[0].input_captures["request"];
        assert!(capture.json.is_none());
        assert_eq!(capture.sequence.len(), 2);
        assert_eq!(
            capture.sequence[1].json.as_ref().unwrap().equals_capture["/request_id"],
            "request"
        );
    }

    #[test]
    fn normalizes_campaign_workflow_input_captures() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
  auditor:
    x-theseus:
      manifest: auditor/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: retry
        input_template: "retry {request}\\n"
        input_captures:
          request:
            pointer: /write_request_id
            workflow:
              pointers: [/request_id]
              stages:
                - service: api
                  steps:
                    - fields:
                        /event: started
                    - fields:
                        /event: write
                - service: auditor
                  pointers: [/write_request_id]
                  steps:
                    - fields:
                        /event: audit
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        let capture = &campaign.operations[0].inputs[0].input_captures["request"];
        assert!(capture.json.is_none());
        assert!(capture.sequence.is_empty());
        let workflow = capture.workflow.as_ref().unwrap();
        assert_eq!(workflow.stages.len(), 2);
        assert_eq!(workflow.stages[1].service, "auditor");
        assert_eq!(workflow.stages[1].pointers, ["/write_request_id"]);
    }

    #[test]
    fn combines_campaign_grammar_choices_with_checkpoint_captures() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: retry
        input_grammar:
          template: "retry {request} {mode}\n"
          name_template: "{mode}"
          choices:
            mode: {normal: normal, force: force}
          input_captures:
            request:
              select: first
              pointer: /request_id
              json:
                fields:
                  /event: write
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        let operation = &campaign.operations[0];
        assert_eq!(
            operation
                .inputs
                .iter()
                .map(|input| input.name.as_str())
                .collect::<Vec<_>>(),
            ["force", "normal"]
        );
        assert_eq!(
            operation.inputs[0].input_template.as_deref(),
            Some("retry {request} force\n")
        );
        assert_eq!(operation.inputs[0].input_hex, "");
        assert_eq!(
            operation.input_grammar.as_ref().unwrap().input_captures["request"].pointer,
            "/request_id"
        );
        assert_eq!(
            operation.input_grammar.as_ref().unwrap().input_captures["request"].select,
            ComposeOperationInputSelect::First
        );
    }

    #[test]
    fn rejects_campaign_input_template_with_unbound_placeholder() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: retry
        input_template: "retry {missing}\n"
        input_captures:
          request:
            pointer: /request_id
            json:
              fields:
                /event: write
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("input_template placeholders must exactly match input_captures"));
    }

    #[test]
    fn rejects_invalid_campaign_operation_input_grammar() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input_grammar:
          template: "write {value}\n"
          choices:
            other: {alpha: alpha}
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("input_grammar has no choices for \"value\""));
    }

    #[test]
    fn rejects_unreachable_campaign_input_case_requirements() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        inputs:
          - name: alpha
            input: "write alpha\n"
            requires: ["write[beta]"]
          - name: beta
            input: "write beta\n"
            requires: ["write[alpha]"]
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("input requirements cannot reach: write[alpha], write[beta]"));
    }

    #[test]
    fn rejects_campaign_state_transitions_for_undeclared_state() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    state: {phase: fresh}
    operations:
      - name: write
        input: "write\n"
        sets_state: {missing: value}
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("sets_state references undeclared state \"missing\""));
    }

    #[test]
    fn rejects_ambiguous_campaign_operation_input_forms() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
        inputs:
          - name: retry
            input: "retry\n"
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("exactly one of input, input_template, inputs, or input_grammar"));
    }

    #[test]
    fn normalizes_campaign_operation_state_rules() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
        max_uses: 1
        requires_markers: [booted]
      - name: close
        input: "close\n"
        requires: [write]
        requires_serial:
          json:
            fields:
              /event: assertion
              /passed: false
            where:
              - pointer: /reason
                equals: stale
        max_uses: 1
      - name: read
        input: "read\n"
        requires: [write]
        excludes: [close]
        excludes_markers: [closed]
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert_eq!(campaign.operations[0].max_uses, Some(1));
        assert_eq!(campaign.operations[0].requires_markers, vec!["booted"]);
        assert_eq!(campaign.operations[2].excludes, vec!["close"]);
        assert_eq!(campaign.operations[2].excludes_markers, vec!["closed"]);
        assert_eq!(
            campaign.operations[1]
                .requires_serial
                .as_ref()
                .unwrap()
                .predicate
                .json
                .as_ref()
                .unwrap()
                .where_[0]
                .equals,
            Some(serde_json::Value::String("stale".to_owned()))
        );
    }

    #[test]
    fn normalizes_operation_barrier_topology_actions() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: write\n        input: 'write\\n'\n      - name: read\n        input: 'read\\n'\n    faults:\n      - kind: partition\n        network: backplane\n        after: write\n      - kind: link_partition\n        network: backplane\n        from: api\n        to: worker\n        after: write\n      - kind: link_heal\n        network: backplane\n        from: api\n        to: worker\n        after: read\n      - kind: storage_fault\n        service: worker\n        drive: data\n        after: write\n        error_ppm: 1000000\n        torn_write_bytes: 1\n      - kind: storage_recover\n        service: worker\n        drive: data\n        after: read\n      - kind: network_fault\n        network: backplane\n        after: write\n        drop_ppm: 1000000\n        latency_rounds: 3\n      - kind: network_recover\n        network: backplane\n        after: read\n",
        );
        let worker = directory.path().join("worker/theseus.toml");
        let input = fs::read_to_string(&worker).unwrap();
        fs::write(
            worker,
            format!("{input}\n[[storage]]\nid = \"data\"\nsize_mib = 1\n"),
        )
        .unwrap();
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert!(matches!(
            campaign.faults[0].kind,
            CampaignFaultKind::Partition
        ));
        assert_eq!(campaign.faults[0].after.as_deref(), Some("write"));
        assert_eq!(campaign.faults[1].from.as_deref(), Some("api"));
        assert_eq!(campaign.faults[1].to.as_deref(), Some("worker"));
        assert!(matches!(
            campaign.faults[2].kind,
            CampaignFaultKind::LinkHeal
        ));
        assert_eq!(campaign.faults[3].drive.as_deref(), Some("data"));
        assert_eq!(campaign.faults[3].error_ppm, Some(1_000_000));
        assert!(matches!(
            campaign.faults[4].kind,
            CampaignFaultKind::StorageRecover
        ));
        assert!(matches!(
            campaign.faults[5].kind,
            CampaignFaultKind::NetworkFault
        ));
        assert_eq!(campaign.faults[5].drop_ppm, Some(1_000_000));
        assert_eq!(campaign.faults[5].latency_rounds, Some(3));
        assert!(matches!(
            campaign.faults[6].kind,
            CampaignFaultKind::NetworkRecover
        ));
    }

    #[test]
    fn normalizes_a_directed_ethertype_matched_packet_fault_and_recovery() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: write\n        input: 'write\\n'\n      - name: retry\n        input: 'retry\\n'\n    faults:\n      - kind: packet_fault\n        network: backplane\n        from: api\n        to: worker\n        after: write\n        ethertype: 0x0800\n        drop_ppm: 1000000\n      - kind: packet_recover\n        network: backplane\n        from: api\n        to: worker\n        after: retry\n        ethertype: 0x0800\n",
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert!(matches!(
            campaign.faults[0].kind,
            CampaignFaultKind::PacketFault
        ));
        assert_eq!(campaign.faults[0].ethertype, Some(0x0800));
        assert_eq!(campaign.faults[0].drop_ppm, Some(1_000_000));
        assert_eq!(campaign.faults[0].from.as_deref(), Some("api"));
        assert_eq!(campaign.faults[0].to.as_deref(), Some("worker"));
        assert!(matches!(
            campaign.faults[1].kind,
            CampaignFaultKind::PacketRecover
        ));
        assert_eq!(campaign.faults[1].ethertype, Some(0x0800));
        assert_eq!(campaign.faults[1].drop_ppm, None);
    }

    #[test]
    fn normalizes_and_validates_clock_rate_faults() {
        let base = r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
      faults:
        - at_round: 2
          kind: clock_jump
          nanoseconds: 1000
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: 'write\n'
    faults:
      - __FAULT__
"#;
        let directory = fixture(
            &base.replace(
                "__FAULT__",
                "kind: clock_rate\n        service: api\n        after: write\n        rate: 4\n        duration_rounds: 32",
            ),
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert!(matches!(
            campaign.faults[0].kind,
            CampaignFaultKind::ClockRate
        ));
        assert_eq!(campaign.faults[0].rate, Some(4));
        assert_eq!(campaign.faults[0].duration_rounds, Some(32));

        let reject = |fault: &str, reason: &str| {
            let fault = fault.replace("\\n", "\n");
            let directory = fixture(&base.replace("__FAULT__", fault.trim()));
            let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
        };
        reject(
            "kind: clock_rate\n        service: api\n        after: write\n        duration_rounds: 32",
            "clock_rate requires rate",
        );
        reject(
            "kind: clock_rate\n        service: api\n        after: write\n        rate: 1\n        duration_rounds: 32",
            "rate must be between 2 and 16",
        );
        reject(
            "kind: clock_rate_release\n        service: api\n        after: write\n        rate: 2",
            "clock_rate_release accepts only service and after",
        );
    }

    #[test]
    fn parses_always_or_unreachable_properties() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: 'write\n'
    properties:
      - name: no_data_loss
        kind: always_or_unreachable
        service: api
        contains: 'THES:ASSERT:no_data_loss:pass'
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert!(matches!(
            campaign.properties[0].kind,
            PropertyKind::AlwaysOrUnreachable
        ));
    }

    #[test]
    fn normalizes_cpu_throttle_and_link_clog_faults() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
  worker:
    x-theseus:
      manifest: worker/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: 'write\n'
      - name: retry
        input: 'retry\n'
    faults:
      - kind: cpu_throttle
        service: worker
        after: write
        duration_rounds: 16
        every_n_rounds: 4
      - kind: cpu_release
        service: worker
        after: retry
      - kind: link_clog
        network: backplane
        from: api
        to: worker
        after: write
        latency_rounds: 64
      - kind: link_unclog
        network: backplane
        from: api
        to: worker
        after: retry
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert!(matches!(
            campaign.faults[0].kind,
            CampaignFaultKind::CpuThrottle
        ));
        assert_eq!(campaign.faults[0].duration_rounds, Some(16));
        assert_eq!(campaign.faults[0].every_n_rounds, Some(4));
        assert!(matches!(
            campaign.faults[1].kind,
            CampaignFaultKind::CpuRelease
        ));
        assert_eq!(campaign.faults[1].duration_rounds, None);
        assert!(matches!(
            campaign.faults[2].kind,
            CampaignFaultKind::LinkClog
        ));
        assert_eq!(campaign.faults[2].latency_rounds, Some(64));
        assert!(matches!(
            campaign.faults[3].kind,
            CampaignFaultKind::LinkUnclog
        ));
        assert_eq!(campaign.faults[3].latency_rounds, None);
    }

    #[test]
    fn rejects_malformed_cpu_throttle_and_link_clog_faults() {
        let base = r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
  worker:
    x-theseus:
      manifest: worker/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: 'write\n'
    faults:
      - __FAULT__
"#;
        let reject = |fault: &str, reason: &str| {
            let fault = fault.replace("\\n", "\n");
            let directory = fixture(&base.replace("__FAULT__", fault.trim()));
            let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
        };
        reject(
            "kind: cpu_throttle\n        service: worker\n        after: write\n        duration_rounds: 16",
            "cpu_throttle requires every_n_rounds",
        );
        reject(
            "kind: cpu_throttle\n        service: worker\n        after: write\n        duration_rounds: 16\n        every_n_rounds: 1",
            "every_n_rounds must be between 2 and 64",
        );
        reject(
            "kind: cpu_throttle\n        service: worker\n        after: write\n        every_n_rounds: 4",
            "cpu_throttle requires duration_rounds",
        );
        reject(
            "kind: cpu_release\n        service: worker\n        after: write\n        every_n_rounds: 4",
            "cpu_release accepts only service and after",
        );
        reject(
            "kind: cpu_throttle\n        service: worker\n        after: write\n        duration_rounds: 16\n        every_n_rounds: 4\n        at_round: 2",
            "cpu_throttle/cpu_release actions accept only service, after, duration_rounds, and every_n_rounds",
        );
        reject(
            "kind: link_clog\n        network: backplane\n        from: api\n        to: worker\n        after: write",
            "link_clog requires latency_rounds",
        );
        reject(
            "kind: link_clog\n        network: backplane\n        from: api\n        to: worker\n        after: write\n        latency_rounds: 10000",
            "latency_rounds must be between 1 and 4096",
        );
        reject(
            "kind: link_unclog\n        network: backplane\n        from: api\n        to: worker\n        after: write\n        latency_rounds: 8",
            "link_unclog accepts only network, from, to, and after",
        );
    }

    #[test]
    fn normalizes_a_campaign_fault_barrier_for_one_input_case() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        inputs:
          - name: alpha
            input: "write alpha\n"
          - name: beta
            input: "write beta\n"
    faults:
      - kind: partition
        network: backplane
        after: "write[beta]"
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert_eq!(campaign.faults[0].after, None);
        assert_eq!(
            campaign.faults[0].after_input.as_ref(),
            Some(&OperationInputReferencePlan {
                operation: "write".to_owned(),
                input: Some("beta".to_owned()),
            })
        );
    }

    #[test]
    fn rejects_a_campaign_with_an_unknown_driver() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: missing\n    operations:\n      - name: request\n        input: 'request\\n'\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("campaign driver"));
    }

    #[test]
    fn rejects_an_unbounded_campaign_fault_sequence() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_faults_per_run: 5\n    operations:\n      - name: request\n        input: 'request\\n'\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("max_faults_per_run"));
    }

    #[test]
    fn normalizes_required_faults_and_enforces_the_schedule_bound() {
        let compose = |maximum| {
            format!(
                r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {{}}
x-theseus:
  campaign:
    driver: api
    max_faults_per_run: {maximum}
    operations:
      - name: write
        input: "write\n"
      - name: read
        input: "read\n"
    faults:
      - kind: partition
        required: true
        network: backplane
        after: write
      - kind: heal
        required: true
        network: backplane
        after: read
"#
            )
        };
        let valid = fixture(&compose(2));
        let campaign = load_compose_plan(valid.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert!(campaign.faults.iter().all(|fault| fault.required));

        let invalid = fixture(&compose(1));
        let error = load_compose_plan(invalid.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("2 required faults"));
        assert!(error.to_string().contains("max_faults_per_run is 1"));
    }

    #[test]
    fn rejects_required_campaign_lifecycle_faults() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: request\n        input: 'request\\n'\n    faults:\n      - kind: pause\n        required: true\n        service: api\n        at_round: 1\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("campaign lifecycle faults cannot be required"));
    }

    #[test]
    fn rejects_an_unbounded_campaign_operation_sequence() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_operations_per_run: 13\n    operations:\n      - name: request\n        input: 'request\\n'\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("max_operations_per_run"));
    }

    #[test]
    fn rejects_unknown_or_cyclic_campaign_operation_requirements() {
        let unknown = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: read
        input: "read\n"
        requires: [write]
"#,
        );
        let error = load_compose_plan(unknown.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("requires unknown operation"));

        let cyclic = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
        requires: [read]
      - name: read
        input: "read\n"
        requires: [write]
"#,
        );
        let error = load_compose_plan(cyclic.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("operation requirements cannot reach"));
    }

    #[test]
    fn rejects_invalid_campaign_operation_state_rules() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
        max_uses: 0
      - name: read
        input: "read\n"
        requires: [write]
        excludes: [write]
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("max_uses must be between 1 and 4"));
    }

    #[test]
    fn rejects_conflicting_campaign_operation_marker_guards() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: read
        input: "read\n"
        requires_markers: [written]
        excludes_markers: [written]
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("both requires and excludes marker"));
    }

    #[test]
    fn rejects_an_invalid_campaign_network_condition() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: request\n        input: 'request\\n'\n    faults:\n      - kind: network_fault\n        network: backplane\n        after: request\n        drop_ppm: 1000001\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("drop_ppm"));
    }

    #[test]
    fn verifies_only_the_named_retained_counterexample() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("campaign-result.json"),
            r#"{"format":"theseus-compose-campaign-result-v1","status":"failed","properties":[{"name":"lost_update","status":"failed"},{"name":"reachable","status":"passed"}]}"#,
        )
        .unwrap();

        verify_counterexample_result(directory.path(), "lost_update", false).unwrap();
        assert!(verify_counterexample_result(directory.path(), "reachable", false).is_err());
        assert!(verify_counterexample_result(directory.path(), "missing", false).is_err());
    }

    #[test]
    fn verifies_a_completed_minimized_counterexample() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("minimization.json"),
            r#"{"property":"lost_update"}"#,
        )
        .unwrap();
        fs::write(directory.path().join("topology-result.json"), "{}").unwrap();

        verify_counterexample_result(directory.path(), "lost_update", true).unwrap();
        assert!(verify_counterexample_result(directory.path(), "other", true).is_err());
    }

    #[test]
    fn resolves_a_locked_runner_relative_to_a_moved_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let artifacts = original.join("artifacts");
        fs::create_dir_all(&artifacts).unwrap();
        let runner = artifacts.join("theseus-topology");
        fs::write(&runner, "runner").unwrap();
        let mut artifact = artifact_for_runner(&runner).unwrap();
        artifact.path = "artifacts/theseus-topology".to_owned();
        fs::write(original.join("replay-plan.json"), "{}").unwrap();
        let moved = directory.path().join("moved");
        fs::rename(original, &moved).unwrap();

        assert_eq!(
            verified_runner(&artifact, &moved.join("replay-plan.json")).unwrap(),
            fs::canonicalize(moved.join("artifacts/theseus-topology")).unwrap()
        );
    }

    fn counterfactual_bundle() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("campaign-result.json"),
            r#"{"format":"theseus-compose-campaign-result-v1","runs":[
                {"index":0,"operations":["write"],"faults":["backplane:partition@write"]},
                {"index":1,"operations":["read"],"fault":"backplane:heal@read"}
            ]}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","campaign":{
                "driver":"api","max_runs":2,"max_faults_per_run":1,"max_operations_per_run":1,
                "operations":[],"faults":[],"properties":[]
            }}"#,
        )
        .unwrap();
        directory
    }

    #[test]
    fn injects_the_locked_counterfactual_into_the_forked_plan() {
        let directory = counterfactual_bundle();
        let plan = forked_counterfactual_plan(
            directory.path(),
            1,
            "backplane:heal@read",
            "backplane:partition@write",
        )
        .unwrap();
        assert_eq!(
            plan["campaign"]["counterfactual"],
            serde_json::json!({
                "run": 1,
                "fault": "backplane:heal@read",
                "replace": "backplane:partition@write",
            })
        );
    }

    #[test]
    fn rejects_counterfactual_faults_that_are_unknown_unchanged_or_unselected() {
        let directory = counterfactual_bundle();

        // The replaced fault must be one the recorded run selected.
        let error = forked_counterfactual_plan(
            directory.path(),
            0,
            "backplane:heal@read",
            "backplane:partition@write",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("run 0 does not select fault \"backplane:heal@read\""),
            "{error}"
        );

        // The replacement must differ from the replaced fault.
        let error = forked_counterfactual_plan(
            directory.path(),
            0,
            "backplane:partition@write",
            "backplane:partition@write",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("replacement must differ from the replaced fault"),
            "{error}"
        );

        // The forked run index must exist in the retained result.
        let error = forked_counterfactual_plan(
            directory.path(),
            7,
            "backplane:partition@write",
            "backplane:heal@read",
        )
        .unwrap_err();
        assert!(error.to_string().contains("no run 7"), "{error}");

        // The legacy single-fault field also validates.
        let error = forked_counterfactual_plan(
            directory.path(),
            1,
            "backplane:partition@write",
            "backplane:heal@read",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("run 1 does not select fault \"backplane:partition@write\""),
            "{error}"
        );
    }

    #[test]
    fn normalizes_a_custom_fault_and_marks_its_service_campaign() {
        let directory = image_fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: request\n        input: 'request\\n'\n    faults:\n      - kind: custom\n        service: api\n        after: request\n        command: [/usr/local/bin/probe, '--flag']\n",
            &[],
        );

        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert!(
            plan.services["api"]
                .run
                .container_service
                .as_ref()
                .unwrap()
                .campaign
        );
        let campaign = plan.campaign.unwrap();
        assert_eq!(campaign.faults.len(), 1);
        let fault = &campaign.faults[0];
        assert!(matches!(fault.kind, CampaignFaultKind::Custom));
        assert_eq!(fault.service.as_deref(), Some("api"));
        assert_eq!(fault.after.as_deref(), Some("request"));
        assert_eq!(
            fault.command.as_deref(),
            Some(&["/usr/local/bin/probe".to_owned(), "--flag".to_owned()][..])
        );
    }

    #[test]
    fn rejects_malformed_custom_faults() {
        let base = "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: request\n        input: 'request\\n'\n    faults:\n";
        let case = |faults: &str| {
            let directory = image_fixture(&format!("{base}{faults}"), &[]);
            load_compose_plan(directory.path().join("compose.yaml")).unwrap_err()
        };

        let error = case("      - kind: custom\n        service: api\n        after: request\n");
        assert!(error.to_string().contains("requires command"), "{error}");

        let error = case("      - kind: custom\n        service: api\n        command: [probe]\n");
        assert!(error.to_string().contains("requires after"), "{error}");

        let error =
            case("      - kind: custom\n        after: request\n        command: [probe]\n");
        assert!(error.to_string().contains("requires service"), "{error}");

        let error = case(
            "      - kind: custom\n        service: ghost\n        after: request\n        command: [probe]\n",
        );
        assert!(
            error.to_string().contains("unknown service \"ghost\""),
            "{error}"
        );

        // worker is manifest-backed and has no container_service contract.
        let error = case(
            "      - kind: custom\n        service: worker\n        after: request\n        command: [probe]\n",
        );
        assert!(
            error
                .to_string()
                .contains("requires image-backed service \"worker\""),
            "{error}"
        );

        let error = case(
            "      - kind: custom\n        service: api\n        after: request\n        command: []\n",
        );
        assert!(error.to_string().contains("non-empty argv"), "{error}");

        let error = case(
            "      - kind: custom\n        service: api\n        after: request\n        command: [probe]\n        network: backplane\n",
        );
        assert!(
            error
                .to_string()
                .contains("accept only service, after, and command"),
            "{error}"
        );

        // A command stays rejected on every other fault kind.
        let error = case(
            "      - kind: partition\n        network: backplane\n        after: request\n        command: [probe]\n",
        );
        assert!(
            error.to_string().contains("accept only network and after"),
            "{error}"
        );

        let error = case(
            "      - kind: custom\n        service: api\n        after: request\n        command: [probe]\n      - kind: custom\n        service: api\n        after: request\n        command: [probe]\n",
        );
        assert!(
            error.to_string().contains("duplicates a custom fault"),
            "{error}"
        );
    }

    #[test]
    fn notification_hooks_observe_retained_status_and_failed_properties() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("campaign");
        fs::create_dir_all(&output).unwrap();
        let result = br#"{"status":"failed","properties":[
            {"name":"lost_update","kind":"always","status":"failed"},
            {"name":"reachable","kind":"reachable","status":"passed"}
        ]}"#;
        fs::write(output.join("campaign-result.json"), result).unwrap();

        let hook = format!("env > {}/hook-env", output.display());
        notify_campaign_completion(Some(&hook), &output);

        let environment = fs::read_to_string(output.join("hook-env")).unwrap();
        assert!(
            environment.contains(&format!("THESEUS_CAMPAIGN_DIR={}", output.display())),
            "{environment}"
        );
        assert!(
            environment.contains("THESEUS_CAMPAIGN_STATUS=failed"),
            "{environment}"
        );
        assert!(
            environment.contains("THESEUS_FAILED_PROPERTIES=lost_update"),
            "{environment}"
        );

        // The hook never changes the retained evidence.
        assert_eq!(
            fs::read(output.join("campaign-result.json")).unwrap(),
            result
        );
    }

    #[test]
    fn notification_hooks_report_failures_without_failing_the_campaign() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("campaign");
        fs::create_dir_all(&output).unwrap();

        // A failing hook writes its marker, reports on stderr, and returns.
        let hook = format!("touch {}/hook-ran; exit 3", output.display());
        notify_campaign_completion(Some(&hook), &output);
        assert!(output.join("hook-ran").is_file());

        // Without retained results the status is unknown rather than an error.
        let hook = format!("env > {}/hook-env", output.display());
        notify_campaign_completion(Some(&hook), &output);
        let environment = fs::read_to_string(output.join("hook-env")).unwrap();
        assert!(
            environment.contains("THESEUS_CAMPAIGN_STATUS=unknown"),
            "{environment}"
        );

        // Without --notify nothing runs at all.
        notify_campaign_completion(None, &output);
        assert!(!output.join("hook-quiet").is_file());
    }
}
