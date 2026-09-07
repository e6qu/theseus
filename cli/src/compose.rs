// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Strict Docker Compose-shaped topology input for Theseus guests.
//!
//! Compose is used here only as a familiar topology notation. Theseus does
//! not accept Docker images, host ports, volumes, or host networks: each
//! service points at its own locked Theseus manifest instead.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use serde_json_path::JsonPath;
use sha2::{Digest, Sha256};

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
    campaign: Option<ComposeCampaign>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeCampaign {
    driver: String,
    #[serde(default)]
    guidance: CampaignGuidance,
    #[serde(default)]
    state: BTreeMap<String, String>,
    operations: Vec<ComposeOperation>,
    #[serde(default)]
    stages: Vec<String>,
    #[serde(default)]
    faults: Vec<ComposeCampaignFault>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeOperation {
    name: String,
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

/// Campaign-only faults. Lifecycle faults occur on scheduler rounds; topology
/// actions run immediately after a named operation reports its UART barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignFaultKind {
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceTheseus {
    manifest: PathBuf,
    #[serde(default)]
    faults: Vec<ComposeFault>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeFault {
    at_round: u64,
    kind: FaultKind,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<u64>,
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
    pub nanoseconds: Option<u64>,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct ComposeServicePlan {
    pub manifest: String,
    pub run: RunPlan,
    pub networks: Vec<String>,
    pub faults: Vec<FaultPlan>,
}

/// A deterministic, serial-driven topology campaign.  Operations are UTF-8
/// UART input for the designated workload service.  The same line protocol is
/// usable from a shell or C program; an SDK is optional.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CampaignGuidance {
    #[default]
    Coverage,
    Adaptive,
}

#[derive(Debug, Clone, Serialize)]
pub struct CampaignPlan {
    pub driver: String,
    #[serde(default)]
    pub guidance: CampaignGuidance,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state: BTreeMap<String, String>,
    pub operations: Vec<OperationPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<String>,
    pub faults: Vec<CampaignFaultPlan>,
    pub properties: Vec<PropertyPlan>,
    pub max_runs: u16,
    pub max_faults_per_run: u8,
    pub max_operations_per_run: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationPlan {
    pub name: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_round: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_rounds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nanoseconds: Option<u64>,
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
    let mut services = BTreeMap::new();
    for (name, service) in compose.services {
        validate_name("service", &name)?;
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
        let run = load_plan(&manifest).map_err(|source| ComposeError::Manifest {
            service: name.clone(),
            source: Box::new(source),
        })?;
        let faults = validate_faults(
            &name,
            service.theseus.faults,
            run.run.virtual_time.is_some(),
        )?;
        services.insert(
            name,
            ComposeServicePlan {
                manifest: manifest.display().to_string(),
                run,
                networks: networks.into_iter().collect(),
                faults,
            },
        );
    }

    let campaign = campaign_plan(compose.theseus, &services)?;
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
    })
}

fn default_campaign_runs() -> u16 {
    32
}

fn default_campaign_faults_per_run() -> u8 {
    2
}

fn default_campaign_operations_per_run() -> u8 {
    3
}

fn campaign_plan(
    campaign: Option<ComposeTheseus>,
    services: &BTreeMap<String, ComposeServicePlan>,
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
    if campaign.operations.is_empty() {
        return Err(ComposeError::Invalid(
            "campaign operations must not be empty".to_owned(),
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
    if campaign.max_operations_per_run == 0 || campaign.max_operations_per_run > 4 {
        return Err(ComposeError::Invalid(
            "campaign max_operations_per_run must be between 1 and 4".to_owned(),
        ));
    }
    let initial_state = campaign.state;
    let evidence_definitions = campaign.evidence;
    let mut resolved_evidence =
        normalize_serial_evidence_definitions(&evidence_definitions, services)?;
    let mut names = BTreeSet::new();
    let mut operations = Vec::with_capacity(campaign.operations.len());
    for operation in campaign.operations {
        validate_name("campaign operation", &operation.name)?;
        if !names.insert(operation.name.clone()) {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} is declared more than once",
                operation.name
            )));
        }
        let input_forms = usize::from(operation.input.is_some())
            + usize::from(operation.input_template.is_some())
            + usize::from(!operation.inputs.is_empty())
            + usize::from(operation.input_grammar.is_some());
        if input_forms > 1 {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} must use exactly one of input, input_template, inputs, or input_grammar",
                operation.name
            )));
        }
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
        let inputs = match operation.input {
            Some(input) if input.is_empty() => {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} has empty input",
                    operation.name
                )));
            }
            Some(input) => vec![OperationInputPlan {
                name: "default".to_owned(),
                input_hex: hex(input.as_bytes()),
                requires: Vec::new(),
                excludes: Vec::new(),
                max_uses: None,
                requires_state: BTreeMap::new(),
                sets_state: BTreeMap::new(),
                input_template: None,
                input_captures: BTreeMap::new(),
            }],
            None if operation.inputs.is_empty()
                && input_grammar.is_none()
                && input_template.is_none() =>
            {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} needs input, input_template, inputs, or input_grammar",
                    operation.name
                )));
            }
            None if input_template.is_some() => {
                let (template, captures) = input_template
                    .as_ref()
                    .expect("input template was normalized");
                vec![OperationInputPlan {
                    name: "default".to_owned(),
                    input_hex: String::new(),
                    input_template: Some(template.clone()),
                    input_captures: captures.clone(),
                    requires: Vec::new(),
                    excludes: Vec::new(),
                    max_uses: None,
                    requires_state: BTreeMap::new(),
                    sets_state: BTreeMap::new(),
                }]
            }
            None if input_grammar.is_some() => input_grammar
                .as_ref()
                .expect("input grammar was normalized")
                .inputs
                .clone(),
            None => {
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
    let mut faults = Vec::with_capacity(campaign.faults.len());
    for candidate in campaign.faults {
        let has_network_conditions = candidate.drop_ppm.is_some()
            || candidate.duplicate_ppm.is_some()
            || candidate.corrupt_ppm.is_some()
            || candidate.jitter_rounds.is_some()
            || candidate.tx_bytes_per_round.is_some()
            || candidate.mtu_bytes.is_some()
            || candidate.tx_queue_frames.is_some()
            || candidate.rx_queue_frames.is_some();
        match candidate.kind {
            CampaignFaultKind::Pause
            | CampaignFaultKind::Restart
            | CampaignFaultKind::ClockJump => {
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
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: None,
                    after: None,
                    after_input: None,
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
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign partition/heal actions accept only network and after".to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    service: None,
                    network: Some(network.to_owned()),
                    from: None,
                    to: None,
                    drive: None,
                    after,
                    after_input,
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
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign link_partition/link_heal actions accept only network, from, to, and after"
                            .to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    service: None,
                    network: Some(network.to_owned()),
                    from: Some(from.to_owned()),
                    to: Some(to.to_owned()),
                    drive: None,
                    after,
                    after_input,
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
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: Some(drive.to_owned()),
                    after,
                    after_input,
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
                });
            }
            CampaignFaultKind::NetworkFault | CampaignFaultKind::NetworkRecover => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign network_fault/network_recover action requires network".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign network_fault/network_recover action requires after".to_owned(),
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
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign network_fault/network_recover actions accept only network, after, and packet-condition fields"
                            .to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::NetworkFault)
                    && !has_network_conditions
                    && candidate.latency_rounds.is_none()
                {
                    return Err(ComposeError::Invalid(
                        "campaign network_fault must set one packet-condition field".to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::NetworkRecover)
                    && (has_network_conditions || candidate.latency_rounds.is_some())
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
                    service: None,
                    network: Some(network.to_owned()),
                    from: None,
                    to: None,
                    drive: None,
                    after,
                    after_input,
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
                    service: None,
                    network: Some(network.to_owned()),
                    from: directed.map(|(from, _)| from.to_owned()),
                    to: directed.map(|(_, to)| to.to_owned()),
                    drive: None,
                    after,
                    after_input,
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
                });
            }
        }
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
        guidance: campaign.guidance,
        state: initial_state,
        operations,
        stages: campaign.stages,
        faults,
        properties,
        max_runs: campaign.max_runs,
        max_faults_per_run: campaign.max_faults_per_run,
        max_operations_per_run: campaign.max_operations_per_run,
    }))
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
                        "service {service:?} clock_jump requires virtual_time, positive nanoseconds, and no duration_rounds"
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
        serde_json::to_vec_pretty(&plan).map_err(|error| {
            ComposeError::Invalid(format!("cannot encode topology plan: {error}"))
        })?,
    )
    .map_err(|source| ComposeError::Read {
        path: plan_file.clone(),
        source,
    })?;
    execute_topology(&plan_file, &output)?;
    let _ = fs::remove_file(&plan_file);
    Ok(output)
}

/// Execute the topology's declared autonomous campaign.  The command uses the
/// same locked-artifact executor as `compose test`; the runner selects
/// campaign mode from the normalized plan instead of accepting host commands.
pub fn explore_compose(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let plan = load_compose_plan(&path)?;
    if plan.campaign.is_none() {
        return Err(ComposeError::Invalid(
            "Compose file has no x-theseus.campaign section".to_owned(),
        ));
    }
    test_compose(path, output)
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

fn execute_topology(plan: &Path, output: &Path) -> Result<(), ComposeError> {
    execute_topology_mode(plan, output, None)
}

fn execute_topology_mode(
    plan: &Path,
    output: &Path,
    mode: Option<&str>,
) -> Result<(), ComposeError> {
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
        .map(verified_runner)
        .transpose()?
        .ok_or_else(|| ComposeError::Invalid("topology replay has no locked executor; replay it with the published runtime that created it".to_owned()))?;
    let status = Command::new(&runner)
        .arg("--plan")
        .arg(plan)
        .arg("--output")
        .arg(output)
        .args(mode)
        .status()
        .map_err(|error| {
            ComposeError::Invalid(format!("cannot start {}: {error}", runner.display()))
        })?;
    if !status.success() {
        return Err(ComposeError::Invalid(format!(
            "topology runner failed; inspect {}",
            output.display()
        )));
    }
    Ok(())
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

fn verified_runner(artifact: &ArtifactPlan) -> Result<PathBuf, ComposeError> {
    let path = PathBuf::from(&artifact.path);
    if artifact_for_runner(&path)?.sha256 != artifact.sha256 {
        return Err(ComposeError::Invalid(format!(
            "topology runner digest changed: {}",
            path.display()
        )));
    }
    Ok(path)
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
    fn rejects_an_unbounded_campaign_operation_sequence() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_operations_per_run: 5\n    operations:\n      - name: request\n        input: 'request\\n'\n",
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
}
