// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Offline, single-file inspection reports for Theseus result directories.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A portable representation of an existing Theseus result directory.
///
/// HTML remains the default for interactive inspection. The other formats
/// deliberately contain the same locked replay recipe and outcome so a CI job,
/// issue, or another program does not have to scrape the browser report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportFormat {
    Html,
    Markdown,
    Json,
    Junit,
}

impl ReportFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "html" => Some(Self::Html),
            "markdown" | "md" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            "junit" | "junit-xml" => Some(Self::Junit),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Junit => "junit",
        }
    }
}

#[derive(Debug)]
pub enum ReportError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    Invalid(String),
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for ReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "cannot parse {}: {source}", path.display())
            }
            Self::Invalid(reason) => formatter.write_str(reason),
            Self::Write { path, source } => {
                write!(formatter, "cannot write {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ReportError {}

#[derive(Deserialize)]
struct ResultRecord {
    format: String,
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    checks: Vec<Check>,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    minimization: Option<Minimization>,
    #[serde(default)]
    replay_verification: Option<ReplayVerification>,
}

#[derive(Clone, Deserialize, Serialize)]
struct ReplayVerification {
    status: String,
    detail: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Minimization {
    original_events_hex: Vec<String>,
    minimized_events_hex: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignMinimization {
    property: String,
    #[serde(default)]
    original_operations: Vec<String>,
    #[serde(default)]
    minimized_operations: Vec<String>,
    #[serde(default)]
    original_faults: Vec<String>,
    #[serde(default)]
    minimized_faults: Vec<String>,
    #[serde(default)]
    operation_attempts: usize,
    #[serde(default)]
    fault_attempts: usize,
}

#[derive(Clone, Deserialize, Serialize)]
struct Check {
    name: String,
    #[serde(default)]
    kind: String,
    status: String,
    detail: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Node {
    search_index: usize,
    id: u64,
    parent: Option<u64>,
    depth: u32,
    seed: u64,
    seed_path: Vec<u64>,
    entropy_probe_hex: String,
    markers_hex: String,
    dirty_pages: Option<u64>,
    #[serde(default)]
    serial_log: Option<String>,
}

#[derive(Deserialize)]
struct TopologyPlan {
    format: String,
    #[serde(default)]
    campaign: Option<CampaignPlan>,
}

#[derive(Deserialize)]
struct TopologyResult {
    #[serde(default)]
    rounds: u64,
    #[serde(default)]
    max_rounds: u64,
    #[serde(default)]
    lifecycle_barrier_rounds: u64,
}

#[derive(Deserialize)]
struct CampaignPlan {
    #[serde(default)]
    state: BTreeMap<String, String>,
    #[serde(default)]
    operations: Vec<CampaignOperation>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignOperation {
    name: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    input_grammar: Option<CampaignOperationInputGrammar>,
    #[serde(default)]
    inputs: Vec<CampaignOperationInput>,
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
    requires_serial: Option<serde_json::Value>,
    #[serde(default)]
    excludes_serial: Option<serde_json::Value>,
    #[serde(default)]
    requires_serial_all: Vec<serde_json::Value>,
    #[serde(default)]
    excludes_serial_any: Vec<serde_json::Value>,
    #[serde(default)]
    requires_serial_joins: Vec<serde_json::Value>,
    #[serde(default)]
    excludes_serial_joins: Vec<serde_json::Value>,
    #[serde(default)]
    requires_serial_evidence: Option<serde_json::Value>,
    #[serde(default)]
    excludes_serial_evidence: Option<serde_json::Value>,
    #[serde(default)]
    max_uses: Option<u8>,
    #[serde(default)]
    requires_state: BTreeMap<String, String>,
    #[serde(default)]
    sets_state: BTreeMap<String, String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignOperationInputGrammar {
    template: String,
    name_template: String,
    choices: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    input_captures: BTreeMap<String, CampaignOperationInputCapture>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignOperationInput {
    name: String,
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

#[derive(Clone, Deserialize, Serialize)]
struct CampaignOperationInputCapture {
    service: Option<String>,
    pointer: String,
    #[serde(default)]
    json: Option<serde_json::Value>,
    #[serde(default)]
    sequence: Vec<serde_json::Value>,
    #[serde(default)]
    workflow: Option<serde_json::Value>,
    #[serde(default)]
    encoding: String,
    #[serde(default)]
    select: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignOperationInputReference {
    operation: String,
    #[serde(default)]
    input: Option<String>,
}

#[derive(Deserialize)]
struct ServiceResult {
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    checks: Vec<Check>,
    #[serde(default)]
    faults: Vec<Fault>,
}

#[derive(Deserialize)]
struct CampaignResult {
    format: String,
    status: String,
    driver: String,
    #[serde(default)]
    guidance: String,
    #[serde(default)]
    checkpoint_nodes: usize,
    #[serde(default)]
    checkpoint_reuses: usize,
    #[serde(default)]
    generated_candidates: usize,
    #[serde(default)]
    marker_guard_rejections: usize,
    #[serde(default)]
    serial_guard_rejections: usize,
    #[serde(default)]
    unique_topology_states: usize,
    #[serde(default)]
    unique_instruction_locations: usize,
    #[serde(default)]
    search: Option<CampaignSearchEvidence>,
    #[serde(default)]
    replay_verification: Option<ReplayVerification>,
    #[serde(default)]
    runs: Vec<CampaignRun>,
    #[serde(default)]
    properties: Vec<CampaignProperty>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignRun {
    index: usize,
    operations: Vec<String>,
    #[serde(default)]
    fault: Option<String>,
    #[serde(default)]
    faults: Vec<String>,
    #[serde(default)]
    actions: Vec<CampaignAction>,
    #[serde(default)]
    selection: String,
    #[serde(default)]
    guidance_ledger: CampaignGuidanceLedger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    guidance_evidence: Option<CampaignPosteriorEvidence>,
    #[serde(default)]
    property_witnesses: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    timeline: Vec<CampaignTimelineBoundary>,
    #[serde(default)]
    program_counters: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    instruction_locations: BTreeMap<String, Vec<InstructionLocation>>,
    #[serde(default)]
    instruction_novelty: Vec<String>,
    #[serde(default)]
    state_novel: bool,
    status: String,
    #[serde(default)]
    novelty: Vec<String>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CampaignCheckpointEconomics {
    #[serde(default)]
    root_captures: usize,
    #[serde(default)]
    prefix_captures: usize,
    #[serde(default)]
    checkpoint_nodes: usize,
    #[serde(default)]
    prefix_reuses: usize,
    #[serde(default)]
    prefix_restores: usize,
    #[serde(default)]
    leaf_restores: usize,
    #[serde(default)]
    topology_restores: usize,
    #[serde(default)]
    avoided_prefix_recomputations: usize,
    #[serde(default)]
    retained_memory_bytes: u64,
    #[serde(default)]
    shared_cow_restore_bytes: u64,
    #[serde(default)]
    private_dirty_pages: u64,
    #[serde(default)]
    snapshot_file_bytes: u64,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CampaignSearchEvidence {
    #[serde(default)]
    checkpoint: CampaignCheckpointEconomics,
    #[serde(default)]
    guidance_observations: usize,
    #[serde(default)]
    guidance_sha256: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CampaignGuidanceLedger {
    #[serde(default)]
    observations: usize,
    #[serde(default)]
    sha256: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignPosteriorEvidence {
    action: String,
    #[serde(default)]
    context: Vec<String>,
    scope: String,
    successes: usize,
    misses: usize,
    mean_per_mille: usize,
    uncertainty_per_mille: usize,
    score: usize,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignTimelineBoundary {
    operation: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    input: CampaignInputEvidence,
    #[serde(default)]
    delivery: CampaignUartDelivery,
    #[serde(default)]
    barrier: CampaignUartBarrier,
    #[serde(default)]
    round: u64,
    #[serde(default)]
    actions: Vec<CampaignAction>,
    #[serde(default)]
    markers: Vec<String>,
    #[serde(default)]
    new_markers: Vec<String>,
    #[serde(default)]
    changed_program_counters: Vec<String>,
    #[serde(default)]
    changed_serial: Vec<String>,
    #[serde(default)]
    program_counters: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    instruction_locations: BTreeMap<String, Vec<InstructionLocation>>,
    #[serde(default)]
    serial_sha256: BTreeMap<String, String>,
    #[serde(default)]
    serial_delta: BTreeMap<String, CampaignSerialDelta>,
    #[serde(default)]
    network_traffic_delta: BTreeMap<String, BTreeMap<String, CampaignNetworkTrafficDelta>>,
    #[serde(default)]
    changed_storage: Vec<String>,
    #[serde(default)]
    virtual_time_delta_ns: BTreeMap<String, Vec<u64>>,
    #[serde(default)]
    state_sha256: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CampaignSerialDelta {
    bytes: usize,
    sha256: String,
    excerpt: String,
    #[serde(default)]
    omitted_bytes: usize,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CampaignInputEvidence {
    #[serde(default)]
    bytes: usize,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    excerpt: String,
    #[serde(default)]
    omitted_bytes: usize,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CampaignUartDelivery {
    #[serde(default)]
    recorded: bool,
    #[serde(default)]
    accepted_bytes: usize,
    #[serde(default)]
    pending_before: usize,
    #[serde(default)]
    pending_after: usize,
    #[serde(default)]
    guest_read_bytes: usize,
    #[serde(default)]
    checkpoint: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CampaignUartBarrier {
    #[serde(default)]
    recorded: bool,
    #[serde(default)]
    checkpoint: String,
    #[serde(default)]
    marker_offset: usize,
    #[serde(default)]
    round: u64,
    #[serde(default)]
    response: CampaignSerialDelta,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignNetworkTrafficDelta {
    #[serde(default)]
    tx_frames: u64,
    #[serde(default)]
    rx_frames: u64,
    #[serde(default)]
    dropped: u64,
    #[serde(default)]
    duplicated: u64,
    #[serde(default)]
    corrupted: u64,
}

#[derive(Clone, Deserialize, Serialize)]
struct InstructionLocation {
    address: String,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    source: Option<InstructionSourceLocation>,
}

#[derive(Clone, Deserialize, Serialize)]
struct InstructionSourceLocation {
    file: String,
    line: u32,
    #[serde(default)]
    column: Option<u32>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignAction {
    kind: String,
    target: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignProperty {
    name: String,
    kind: String,
    status: String,
    detail: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Fault {
    round: u64,
    kind: String,
    detail: String,
    #[serde(default)]
    barrier_rounds: Option<u64>,
}

#[derive(Serialize)]
struct ReportModel {
    title: String,
    kind: String,
    status: String,
    error: Option<String>,
    command_label: String,
    command: String,
    path_command: Option<String>,
    minimize_path_command: Option<String>,
    snapshot_path_command: Option<String>,
    checks: Vec<Check>,
    faults: Vec<Fault>,
    logs: Vec<Log>,
    nodes: Vec<Node>,
    coverage: Option<Coverage>,
    minimization: Option<Minimization>,
    campaign_minimization: Option<CampaignMinimization>,
    replay_verification: Option<ReplayVerification>,
    campaign_runs: Vec<CampaignRun>,
    campaign_state: BTreeMap<String, String>,
    campaign_operations: Vec<CampaignOperation>,
}

#[derive(Serialize)]
struct Log {
    label: String,
    text: String,
}

#[derive(Serialize)]
struct Coverage {
    label: String,
    summary: String,
}

/// Render `input` into a standalone `index.html` under `output`.
///
/// The input is a completed or failed single-timeline replay, topology replay,
/// or exploration directory. All report data comes from files inside that
/// directory; symlinks outside it are rejected.
pub fn report(input: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<PathBuf, ReportError> {
    let root = report_root(input.as_ref())?;
    let output = output.as_ref();
    if output.exists() {
        return Err(ReportError::Invalid(format!(
            "report output already exists: {}",
            output.display()
        )));
    }
    let model = load_model(&root)?;
    fs::create_dir_all(output).map_err(|source| ReportError::Write {
        path: output.to_path_buf(),
        source,
    })?;
    let index = output.join("index.html");
    fs::write(&index, render(&model)?).map_err(|source| ReportError::Write {
        path: index.clone(),
        source,
    })?;
    Ok(index)
}

/// Render a report in a portable text format. Unlike [`report`], this does
/// not create files, which makes Markdown suitable for `$GITHUB_STEP_SUMMARY`
/// and JSON suitable for a pipe.
pub fn report_text(input: impl AsRef<Path>, format: ReportFormat) -> Result<String, ReportError> {
    if format == ReportFormat::Html {
        return Err(ReportError::Invalid(
            "HTML reports need an output directory; use `theseus report`".to_owned(),
        ));
    }
    let root = report_root(input.as_ref())?;
    render_format(&load_model(&root)?, format)
}

/// Write a portable text report to a new file.
pub fn report_file(
    input: impl AsRef<Path>,
    format: ReportFormat,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ReportError> {
    if format == ReportFormat::Html {
        return report(input, output);
    }
    let output = output.as_ref();
    if output.exists() {
        return Err(ReportError::Invalid(format!(
            "report output already exists: {}",
            output.display()
        )));
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ReportError::Write {
        path: parent.to_path_buf(),
        source,
    })?;
    fs::write(output, report_text(input, format)?).map_err(|source| ReportError::Write {
        path: output.to_path_buf(),
        source,
    })?;
    Ok(output.to_path_buf())
}

fn report_root(input: &Path) -> Result<PathBuf, ReportError> {
    let root = fs::canonicalize(input).map_err(|source| ReportError::Read {
        path: input.to_path_buf(),
        source,
    })?;
    if !root.is_dir() {
        return Err(ReportError::Invalid(format!(
            "report input is not a directory: {}",
            root.display()
        )));
    }
    Ok(root)
}

fn load_model(root: &Path) -> Result<ReportModel, ReportError> {
    let result_path = root.join("result.json");
    if result_path.is_file() {
        let result: ResultRecord = read_json(root, Path::new("result.json"))?;
        return match result.format.as_str() {
            "theseus-result-v1" => single_timeline(root, result),
            "theseus-exploration-result-v1" => exploration(root, result),
            format => Err(ReportError::Invalid(format!(
                "unsupported Theseus result format {format:?}"
            ))),
        };
    }
    topology(root)
}

fn single_timeline(root: &Path, result: ResultRecord) -> Result<ReportModel, ReportError> {
    Ok(ReportModel {
        title: "Timeline replay".to_owned(),
        kind: "one deterministic timeline".to_owned(),
        status: result.status,
        error: result.error,
        command_label: "Replay this locked bundle".to_owned(),
        command: format!("theseus replay {}", shell_quote(root)),
        path_command: None,
        minimize_path_command: None,
        snapshot_path_command: None,
        checks: result.checks,
        faults: Vec::new(),
        logs: maybe_log(root, Path::new("serial.log"), "Serial log")?
            .into_iter()
            .collect(),
        nodes: Vec::new(),
        coverage: None,
        minimization: None,
        campaign_minimization: None,
        replay_verification: result.replay_verification,
        campaign_runs: Vec::new(),
        campaign_state: BTreeMap::new(),
        campaign_operations: Vec::new(),
    })
}

fn exploration(root: &Path, mut result: ResultRecord) -> Result<ReportModel, ReportError> {
    result.nodes.sort_by_key(|node| node.search_index);
    let _: serde_json::Value = read_json(root, Path::new("explore-plan.json"))?;
    let populated = result
        .nodes
        .iter()
        .filter_map(|node| node.dirty_pages)
        .collect::<Vec<_>>();
    let coverage = if populated.is_empty() {
        None
    } else {
        let unique = populated
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        Some(Coverage {
            label: "Dirty-page footprint (coverage proxy)".to_owned(),
            summary: format!(
                "{} captured nodes; {} distinct dirty-page counts; range {}–{} pages",
                populated.len(),
                unique.len(),
                populated.iter().min().unwrap(),
                populated.iter().max().unwrap()
            ),
        })
    };
    let logs = result
        .nodes
        .iter()
        .filter_map(|node| {
            node.serial_log.as_deref().map(|serial_log| {
                maybe_log(
                    root,
                    Path::new(serial_log),
                    &format!("Timeline #{} serial log", node.search_index),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(ReportModel {
        title: "Exploration".to_owned(),
        kind: "deterministic timeline search".to_owned(),
        status: result.status,
        error: result.error,
        command_label: "Replay this locked exploration".to_owned(),
        command: format!(
            "theseus explore --replay {} --output exploration-rerun",
            shell_quote(root)
        ),
        path_command: Some(format!(
            "theseus explore --replay {} --seed-path ",
            shell_quote(root)
        )),
        minimize_path_command: Some(format!(
            "theseus explore --minimize {} --seed-path ",
            shell_quote(root)
        )),
        snapshot_path_command: Some(format!(
            "theseus explore --snapshot {} --seed-path ",
            shell_quote(root)
        )),
        checks: result.checks,
        faults: Vec::new(),
        logs,
        nodes: result.nodes,
        coverage,
        minimization: result.minimization,
        campaign_minimization: None,
        replay_verification: result.replay_verification,
        campaign_runs: Vec::new(),
        campaign_state: BTreeMap::new(),
        campaign_operations: Vec::new(),
    })
}

fn topology(root: &Path) -> Result<ReportModel, ReportError> {
    let plan: TopologyPlan = read_json(root, Path::new("replay-plan.json"))?;
    if plan.format != "theseus-compose-plan-v1" {
        return Err(ReportError::Invalid(
            "directory has neither a Theseus result nor a topology replay plan".to_owned(),
        ));
    }
    if root.join("campaign-result.json").is_file() {
        return campaign(root);
    }
    let campaign_minimization = root
        .join("minimization.json")
        .is_file()
        .then(|| read_json(root, Path::new("minimization.json")))
        .transpose()?;
    let topology_budget = root
        .join("topology-result.json")
        .is_file()
        .then(|| read_json::<TopologyResult>(root, Path::new("topology-result.json")))
        .transpose()?;
    let services = root.join("services");
    let entries = fs::read_dir(&services).map_err(|source| ReportError::Read {
        path: services.clone(),
        source,
    })?;
    let mut results = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|source| ReportError::Read {
            path: services.clone(),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = PathBuf::from("services").join(&name).join("result.json");
        if local_path(root, &relative).is_ok_and(|path| path.is_file()) {
            results.insert(name, read_json::<ServiceResult>(root, &relative)?);
        }
    }
    if results.is_empty() {
        return Err(ReportError::Invalid(format!(
            "topology replay has no service results: {}",
            services.display()
        )));
    }
    let mut checks = Vec::new();
    let mut faults = Vec::new();
    let mut logs = Vec::new();
    let mut failed = false;
    let mut errors = Vec::new();
    for (service, result) in results {
        failed |= result.status != "passed";
        if let Some(error) = result.error {
            errors.push(format!("{service}: {error}"));
        }
        checks.extend(result.checks.into_iter().map(|mut check| {
            check.name = format!("{service}: {}", check.name);
            check
        }));
        faults.extend(result.faults.into_iter().map(|mut fault| {
            fault.detail = format!("{service}: {}", fault.detail);
            fault
        }));
        let service_dir = PathBuf::from("services").join(&service);
        logs.extend(service_logs(root, &service_dir, &service)?);
    }
    Ok(ReportModel {
        title: "Topology replay".to_owned(),
        kind: "deterministic service topology".to_owned(),
        status: if failed { "failed" } else { "passed" }.to_owned(),
        error: (!errors.is_empty()).then(|| errors.join("\n")),
        command_label: "Replay this locked topology".to_owned(),
        command: format!(
            "theseus compose replay {} --output topology-rerun",
            shell_quote(root)
        ),
        path_command: None,
        minimize_path_command: None,
        snapshot_path_command: None,
        checks,
        faults,
        logs,
        nodes: Vec::new(),
        coverage: topology_budget.map(|budget| Coverage {
            label: "Topology execution".to_owned(),
            summary: format!(
                "{} of {} deterministic scheduler rounds consumed; {} lifecycle barrier rounds",
                budget.rounds, budget.max_rounds, budget.lifecycle_barrier_rounds
            ),
        }),
        minimization: None,
        campaign_minimization,
        replay_verification: None,
        campaign_runs: Vec::new(),
        campaign_state: BTreeMap::new(),
        campaign_operations: Vec::new(),
    })
}

fn campaign(root: &Path) -> Result<ReportModel, ReportError> {
    let result: CampaignResult = read_json(root, Path::new("campaign-result.json"))?;
    let plan: TopologyPlan = read_json(root, Path::new("replay-plan.json"))?;
    if result.format != "theseus-compose-campaign-result-v1" {
        return Err(ReportError::Invalid(format!(
            "unsupported campaign result format {:?}",
            result.format
        )));
    }
    let checks = result
        .properties
        .iter()
        .cloned()
        .map(|property| Check {
            name: property.name,
            kind: property.kind,
            status: property.status,
            detail: property.detail,
        })
        .collect();
    let (campaign_state, campaign_operations) = plan
        .campaign
        .map(|campaign| (campaign.state, campaign.operations))
        .unwrap_or_else(|| (BTreeMap::new(), Vec::new()));
    let guidance = match result.guidance.as_str() {
        "adaptive" => "adaptive coverage and observed action-yield guidance",
        "posterior" => "posterior coverage and action-yield guidance",
        "property" => "declared-property and coverage guidance",
        _ => "marker, instruction-location, and topology-state coverage",
    };
    let checkpoint = result
        .search
        .as_ref()
        .map(|search| &search.checkpoint)
        .cloned()
        .unwrap_or_else(|| CampaignCheckpointEconomics {
            checkpoint_nodes: result.checkpoint_nodes,
            prefix_reuses: result.checkpoint_reuses,
            ..CampaignCheckpointEconomics::default()
        });
    let guidance_ledger = result.search.as_ref().map(|search| {
        format!(
            "; guidance ledger: {} observations, sha256 {}",
            search.guidance_observations, search.guidance_sha256
        )
    });
    Ok(ReportModel {
        title: "Autonomous Compose campaign".to_owned(),
        kind: format!("deterministic topology search driven by {}", result.driver),
        status: result.status,
        error: None,
        command_label: "Replay this locked campaign".to_owned(),
        command: format!(
            "theseus compose replay {} --output campaign-rerun",
            shell_quote(root)
        ),
        path_command: None,
        minimize_path_command: None,
        snapshot_path_command: None,
        checks,
        faults: Vec::new(),
        logs: Vec::new(),
        nodes: Vec::new(),
        coverage: Some(Coverage {
            label: "Campaign corpus".to_owned(),
            summary: format!(
                "{} of {} deterministic candidates selected by {guidance}; {} marker-guard leaves and {} serial-guard leaves skipped; {} unique instruction locations; {} unique topology states; {} root captures, {} reusable checkpoint nodes, {} prefix captures, {} prefix reuses ({} avoided recomputations), {} topology restores ({} prefix materializations + {} leaf replays); {} retained immutable bytes, {} logical COW-mapped restore bytes, {} dirty pages at capture barriers, {} snapshot-file bytes{}",
                result.runs.len(),
                result.generated_candidates,
                result.marker_guard_rejections,
                result.serial_guard_rejections,
                result.unique_instruction_locations,
                result.unique_topology_states,
                checkpoint.root_captures,
                checkpoint.checkpoint_nodes,
                checkpoint.prefix_captures,
                checkpoint.prefix_reuses,
                checkpoint.avoided_prefix_recomputations,
                checkpoint.topology_restores,
                checkpoint.prefix_restores,
                checkpoint.leaf_restores,
                checkpoint.retained_memory_bytes,
                checkpoint.shared_cow_restore_bytes,
                checkpoint.private_dirty_pages,
                checkpoint.snapshot_file_bytes,
                guidance_ledger.unwrap_or_default(),
            ),
        }),
        minimization: None,
        campaign_minimization: None,
        replay_verification: result.replay_verification,
        campaign_runs: result.runs,
        campaign_state,
        campaign_operations,
    })
}

fn service_logs(root: &Path, directory: &Path, service: &str) -> Result<Vec<Log>, ReportError> {
    let path = local_path(root, directory)?;
    let entries = fs::read_dir(&path).map_err(|source| ReportError::Read {
        path: path.clone(),
        source,
    })?;
    let mut names = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name == "serial.log" || (name.starts_with("serial-") && name.ends_with(".log"))
        })
        .collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let relative = directory.join(&name);
            Ok(Log {
                label: format!("{service}: {name}"),
                text: read_text(root, &relative)?,
            })
        })
        .collect()
}

fn maybe_log(root: &Path, relative: &Path, label: &str) -> Result<Option<Log>, ReportError> {
    match local_path(root, relative) {
        Ok(path) if path.is_file() => Ok(Some(Log {
            label: label.to_owned(),
            text: read_text(root, relative)?,
        })),
        Ok(_) => Ok(None),
        Err(ReportError::Read { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn read_json<T: for<'a> Deserialize<'a>>(root: &Path, relative: &Path) -> Result<T, ReportError> {
    let path = local_path(root, relative)?;
    let bytes = fs::read(&path).map_err(|source| ReportError::Read {
        path: path.clone(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| ReportError::Parse { path, source })
}

fn read_text(root: &Path, relative: &Path) -> Result<String, ReportError> {
    let path = local_path(root, relative)?;
    let bytes = fs::read(&path).map_err(|source| ReportError::Read {
        path: path.clone(),
        source,
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn local_path(root: &Path, relative: &Path) -> Result<PathBuf, ReportError> {
    if relative.is_absolute() || relative.components().any(|part| part.as_os_str() == "..") {
        return Err(ReportError::Invalid(format!(
            "report path escapes its result directory: {}",
            relative.display()
        )));
    }
    let requested = root.join(relative);
    let path = fs::canonicalize(&requested).map_err(|source| ReportError::Read {
        path: requested,
        source,
    })?;
    if !path.starts_with(root) {
        return Err(ReportError::Invalid(format!(
            "report path escapes its result directory: {}",
            relative.display()
        )));
    }
    Ok(path)
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

fn render(model: &ReportModel) -> Result<String, ReportError> {
    let data = serde_json::to_string(model)
        .map_err(|error| ReportError::Invalid(format!("cannot encode report data: {error}")))?
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    Ok(format!(
        r##"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Theseus report</title>
<style>
:root {{ color-scheme: light dark; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }}
body {{ max-width: 1080px; margin: 2rem auto; padding: 0 1rem; line-height: 1.45; }}
h1 {{ margin-bottom: 0; }} .muted {{ color: #667085; }} .status {{ font-weight: bold; }}
.passed {{ color: #157347; }} .failed {{ color: #b42318; }} section {{ border-top: 1px solid #98a2b3; margin-top: 1.5rem; }}
pre {{ overflow: auto; padding: 1rem; background: #101828; color: #f2f4f7; }}
table {{ width: 100%; border-collapse: collapse; }} th, td {{ text-align: left; vertical-align: top; padding: .45rem; border-bottom: 1px solid #d0d5dd; }}
.node {{ border-left: 2px solid #98a2b3; margin: .45rem 0; padding-left: .7rem; }} .node p {{ margin: .2rem 0; }}
</style><body><main id="report"></main>
<script id="report-data" type="application/json">{data}</script>
<script>
const m=JSON.parse(document.querySelector('#report-data').textContent), app=document.querySelector('#report');
const el=(tag,text)=>{{const x=document.createElement(tag);if(text!==undefined)x.textContent=text;return x}};
const section=(title)=>{{const s=el('section'),h=el('h2',title);s.append(h);app.append(s);return s}};
const table=(rows,heads)=>{{const t=el('table'),tr=el('tr');heads.forEach(h=>tr.append(el('th',h)));t.append(tr);rows.forEach(row=>{{const r=el('tr');row.forEach(value=>r.append(el('td',value)));t.append(r)}});return t}};
app.append(el('h1',m.title)); app.append(el('p',m.kind));
const status=el('p','Status: '+m.status);status.className='status '+m.status;app.append(status);
if(m.error){{const e=section('Error');e.append(el('pre',m.error));}}
const replay=section(m.command_label);replay.append(el('pre',m.command));
if(m.nodes.length){{const s=section('Timeline tree');m.nodes.forEach(n=>{{const d=el('div');d.className='node';d.style.marginLeft=(n.depth*1.25)+'rem';d.append(el('strong','#'+n.search_index+' · node '+n.id+' · seed '+n.seed));d.append(el('p','parent: '+(n.parent===null?'root':n.parent)+' · seed path: '+n.seed_path.join(' → ')));if(m.path_command){{d.append(el('code',m.path_command+n.seed_path.join(',')));}}if(m.snapshot_path_command){{d.append(el('p','Export this paused timeline:'));d.append(el('code',m.snapshot_path_command+n.seed_path.join(',')));}}if(m.minimize_path_command&&m.status==='failed'){{d.append(el('p','Minimize this failing path:'));d.append(el('code',m.minimize_path_command+n.seed_path.join(',')));}}d.append(el('p','markers: '+(n.markers_hex||'none')+' · dirty pages: '+(n.dirty_pages===null?'not captured':n.dirty_pages)));if(n.serial_log){{d.append(el('p','serial log: '+n.serial_log));}}d.append(el('p','entropy probe: '+n.entropy_probe_hex));s.append(d)}});}}
if(m.coverage){{const s=section(m.coverage.label);s.append(el('p',m.coverage.summary));}}
if(Object.keys(m.campaign_state).length){{const s=section('Campaign state machine');s.append(el('pre',JSON.stringify(m.campaign_state)));const rows=[];m.campaign_operations.forEach(o=>{{if(Object.keys(o.requires_state).length||Object.keys(o.sets_state).length)rows.push([o.name,JSON.stringify(o.requires_state),JSON.stringify(o.sets_state)]);o.inputs.forEach(i=>{{if(Object.keys(i.requires_state).length||Object.keys(i.sets_state).length)rows.push([o.name+'['+i.name+']',JSON.stringify(i.requires_state),JSON.stringify(i.sets_state)]);}});}});if(rows.length)s.append(table(rows,['Transition','Requires state','Sets state']));}}
if(m.campaign_operations.length){{const predicate=p=>p?JSON.stringify(p):'none',predicates=ps=>ps.length?JSON.stringify(ps):'none',ref=r=>r.operation+(r.input?'['+r.input+']':''),capture=(n,c)=>n+'@'+(c.service||'driver')+':'+c.pointer+' · '+JSON.stringify(c.json||c.workflow||{{sequence:c.sequence}})+' ('+(c.encoding||'text')+', '+(c.select||'latest')+')',input=i=>{{const rules=i.requires.length||i.excludes.length||i.max_uses!==null?' ('+[i.requires.length?'after '+i.requires.map(ref).join(' + '):'',i.excludes.length?'without '+i.excludes.map(ref).join(' + '):'',i.max_uses===null?'':'at most '+i.max_uses].filter(Boolean).join('; ')+')':'';const captures=i.input_template?' ← '+i.input_template+' · '+Object.entries(i.input_captures).map(([n,c])=>capture(n,c)).join(', '):'';return i.name+rules+captures}},grammar=o=>o.input_grammar?(o.input_grammar.name_template+' ← '+o.input_grammar.template+' · '+Object.entries(o.input_grammar.choices).map(([v,c])=>v+'='+Object.keys(c).join('/')).join(', ')+(Object.keys(o.input_grammar.input_captures).length?' · '+Object.entries(o.input_grammar.input_captures).map(([n,c])=>capture(n,c)).join(', '):'')):'literal cases',s=section('Operation model');s.append(table(m.campaign_operations.map(o=>[o.name,grammar(o),o.inputs.map(input).join(' + ')||'default',o.stage||'any',o.requires.join(' + ')||'none',o.excludes.join(' + ')||'none',o.requires_markers.join(' + ')||'none',o.excludes_markers.join(' + ')||'none',predicate(o.requires_serial),predicate(o.excludes_serial),predicates(o.requires_serial_all),predicates(o.excludes_serial_any),predicates(o.requires_serial_joins),predicates(o.excludes_serial_joins),predicate(o.requires_serial_evidence),predicate(o.excludes_serial_evidence),o.max_uses===null?'unbounded':String(o.max_uses)]),['Operation','Input grammar','Input cases','Stage','Requires earlier','Excludes earlier','Requires observed marker','Excludes observed marker','Requires serial predicate','Excludes serial predicate','Requires all serial guards','Excludes any serial guard','Requires JSON joins','Excludes JSON joins','Requires serial evidence','Excludes serial evidence','Maximum uses']));}}
if(m.campaign_operations.some(o=>o.service)){{const s=section('Operation targets');s.append(el('p','Each operation sends its UART input to this service. Operations without a target in older bundles use the designated campaign driver.'));s.append(table(m.campaign_operations.filter(o=>o.service).map(o=>[o.name,o.service]),['Operation','Service']));}}
if(m.campaign_runs.length){{const location=l=>{{if(typeof l==='string')return l;const label=l.address+(l.symbol?' → '+l.symbol+(l.offset?' +0x'+l.offset.toString(16):''):'');return l.source?label+' · '+l.source.file+':'+l.source.line+(l.source.column?':'+l.source.column:''):label}},locations=r=>Object.entries(r.program_counters).map(([service,pcs])=>service+': '+((r.instruction_locations[service]||pcs).map(location).join(' '))).join(' · ')||'none',ledger=r=>r.guidance_ledger&&r.guidance_ledger.sha256?r.guidance_ledger.observations+' observations · '+r.guidance_ledger.sha256:'unrecorded (legacy)',posterior=r=>{{const p=r.guidance_evidence;return p.scope+' · '+p.successes+' yield(s), '+p.misses+' miss(es) · mean '+p.mean_per_mille+'‰ + '+p.uncertainty_per_mille+'‰'}},hasPosterior=m.campaign_runs.some(r=>r.guidance_evidence),rows=m.campaign_runs.map(r=>{{const row=[String(r.index),r.operations.join(' → ')||'none',(r.faults.length?r.faults:(r.fault?[r.fault]:[])).join(' + ')||'none',r.selection||'canonical breadth-first seed',ledger(r)];if(hasPosterior)row.push(posterior(r));row.push(r.state_novel?'new':'seen',locations(r),r.actions.map(a=>a.kind+' '+a.target).join(' · ')||'none',r.status,r.novelty.join(' ')||'none');return row}}),heads=['Run','Operations','Candidates','Selection','Guidance ledger'];if(hasPosterior)heads.push('Posterior evidence');heads.push('Topology state','Instruction locations','Applied actions','Status','New markers');const s=section('Generated timelines');s.append(table(rows,heads));}}
if(m.campaign_runs.some(r=>r.property_witnesses.length)){{const rows=m.campaign_runs.filter(r=>r.property_witnesses.length).map(r=>[String(r.index),r.operations.join(' → ')||'none',r.property_witnesses.join(', ')]),s=section('Property witnesses');s.append(el('p','These declared properties produced useful evidence in this timeline. A reachable or sometimes match is a witness; an always or unreachable witness is a counterexample.'));s.append(table(rows,['Run','Operations','Property witnesses']));}}
if(m.campaign_runs.some(r=>r.timeline.length)){{
const location=l=>{{const label=l.address+(l.symbol?' → '+l.symbol+(l.offset?' +0x'+l.offset.toString(16):''):'');return l.source?label+' · '+l.source.file+':'+l.source.line+(l.source.column?':'+l.source.column:''):label}},
locations=b=>Object.entries(b.program_counters).map(([service,pcs])=>service+': '+((b.instruction_locations[service]||pcs).map(location).join(' '))).join(' · ')||'none',
delta=b=>[['new markers',b.new_markers.join(' ')],['changed PCs',b.changed_program_counters.join(', ')],['changed serial',b.changed_serial.join(', ')]].filter(([,value])=>value).map(([label,value])=>label+': '+value).join('; ')||'none',
serial=b=>Object.entries(b.serial_delta).map(([service,d])=>service+': '+d.excerpt+' ['+d.bytes+' bytes; sha256 '+d.sha256+(d.omitted_bytes?'; +'+d.omitted_bytes+' bytes':'')+']').join(' · ')||'none',
input=b=>b.input.sha256?b.input.excerpt+' ['+b.input.bytes+' bytes; sha256 '+b.input.sha256+(b.input.omitted_bytes?'; +'+b.input.omitted_bytes+' bytes':'')+']':'unrecorded (legacy)',
delivery=b=>b.delivery.recorded?'accepted '+b.delivery.accepted_bytes+' bytes; guest read '+b.delivery.guest_read_bytes+'; queued '+b.delivery.pending_before+' → '+b.delivery.pending_after+(b.delivery.checkpoint?' · waited for '+b.delivery.checkpoint:' · no marker barrier'):'unrecorded (legacy)',
barrier=b=>b.barrier.recorded?b.barrier.checkpoint+' at round '+b.barrier.round+', +'+b.barrier.marker_offset+' · '+b.barrier.response.excerpt+' ['+b.barrier.response.bytes+' bytes; sha256 '+b.barrier.response.sha256+(b.barrier.response.omitted_bytes?'; +'+b.barrier.response.omitted_bytes+' bytes':'')+']':'unrecorded (legacy)',
traffic=b=>Object.entries(b.network_traffic_delta).flatMap(([service,nets])=>Object.entries(nets).map(([network,d])=>service+'.'+network+': tx '+d.tx_frames+' rx '+d.rx_frames+' drop '+d.dropped+' dup '+d.duplicated+' corrupt '+d.corrupted)).join(' · ')||'none',
storage=b=>b.changed_storage.join(', ')||'none',
virtualTime=b=>Object.entries(b.virtual_time_delta_ns).map(([service,clocks])=>service+': '+clocks.join(', ')+' ns').join(' · ')||'none',
rows=m.campaign_runs.flatMap(r=>r.timeline.map(b=>[String(r.index),b.operation,b.service||'driver (legacy)',input(b),delivery(b),barrier(b),String(b.round),delta(b),b.markers.join(' ')||'none',locations(b),b.actions.map(a=>a.kind+' '+a.target).join(' · ')||'none',Object.entries(b.serial_sha256).map(([service,hash])=>service+':'+hash).join(' ')||'none',serial(b),traffic(b),storage(b),virtualTime(b),b.state_sha256||'none'])),
s=section('Operation boundaries');
s.append(el('p','Each row is the paused checkpoint after one operation. Target names the service whose UART received it. UART input is an escaped, bounded copy of the exact delivered bytes; its hash covers the complete input in the locked replay plan. UART delivery records accepted bytes, guest FIFO reads, and queued bytes. UART barrier proves the named marker arrived after that input, with its post-input response hash and excerpt. The delta compares the checkpoint with the preceding one. New serial output is also escaped and bounded. Network counters, changed storage, and virtual-time deltas show state produced by this operation.'));
s.append(table(rows,['Run','Operation','Target','UART input','UART delivery','UART barrier','Round','Delta','Markers','Instruction locations','Applied actions','Serial SHA-256','New serial output','Network traffic','Changed storage','Virtual time delta','State SHA-256']));
}}
if(m.minimization){{const s=section('Event minimization');s.append(table([[m.minimization.original_events_hex.join(' ')||'none',m.minimization.minimized_events_hex.join(' ')||'none']],['Original events','1-minimal events']));}}
if(m.campaign_minimization){{const x=m.campaign_minimization,s=section('Campaign minimization');s.append(table([[x.property,x.original_operations.join(' → ')||'none',x.minimized_operations.join(' → ')||'none',x.original_faults.join(' + ')||'none',x.minimized_faults.join(' + ')||'none',String(x.operation_attempts),String(x.fault_attempts)]],['Property','Original operations','1-minimal operations','Original faults','1-minimal faults','Operation replays','Fault replays']));}}
if(m.replay_verification){{const s=section('Replay verification');s.append(table([[m.replay_verification.status,m.replay_verification.detail]],['Status','Detail']));}}
if(m.checks.length){{const s=section('Checks');s.append(table(m.checks.map(c=>[c.name,c.kind,c.status,c.detail]),['Name','Kind','Status','Detail']));}}
if(m.faults.length){{const s=section('Applied faults');s.append(table(m.faults.map(f=>[String(f.round),f.kind,f.detail,f.barrier_rounds===null?'—':String(f.barrier_rounds)]),['Round','Kind','Detail','Barrier rounds']));}}
if(m.logs.length){{const s=section('Logs');m.logs.forEach(log=>{{s.append(el('h3',log.label));s.append(el('pre',log.text));}});}}
</script></body></html>"##
    ))
}

fn render_format(model: &ReportModel, format: ReportFormat) -> Result<String, ReportError> {
    match format {
        ReportFormat::Html => render(model),
        ReportFormat::Markdown => Ok(render_markdown(model)),
        ReportFormat::Json => serde_json::to_string_pretty(&MachineReport::new(model))
            .map_err(|error| ReportError::Invalid(format!("cannot encode report data: {error}"))),
        ReportFormat::Junit => Ok(render_junit(model)),
    }
}

#[derive(Serialize)]
struct MachineReport<'a> {
    format: &'static str,
    #[serde(flatten)]
    report: &'a ReportModel,
}

impl<'a> MachineReport<'a> {
    fn new(report: &'a ReportModel) -> Self {
        Self {
            format: "theseus-report-v1",
            report,
        }
    }
}

fn markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', "<br>")
}

fn instruction_location_label(location: &InstructionLocation) -> String {
    let label = match (&location.symbol, location.offset) {
        (Some(symbol), Some(offset)) if offset != 0 => {
            format!("{} → {symbol} +0x{offset:x}", location.address)
        }
        (Some(symbol), _) => format!("{} → {symbol}", location.address),
        (None, _) => location.address.clone(),
    };
    match &location.source {
        Some(source) => format!(
            "{label} · {}:{}{}",
            source.file,
            source.line,
            source
                .column
                .map(|column| format!(":{column}"))
                .unwrap_or_default()
        ),
        None => label,
    }
}

fn campaign_instruction_location_labels(run: &CampaignRun) -> String {
    instruction_location_labels(&run.program_counters, &run.instruction_locations)
}

fn instruction_location_labels(
    program_counters: &BTreeMap<String, Vec<String>>,
    instruction_locations: &BTreeMap<String, Vec<InstructionLocation>>,
) -> String {
    program_counters
        .iter()
        .map(|(service, counters)| {
            let locations = instruction_locations
                .get(service)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let labels = if locations.is_empty() {
                counters.clone()
            } else {
                locations.iter().map(instruction_location_label).collect()
            };
            format!("{}: {}", service, labels.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn campaign_serial_delta_label(boundary: &CampaignTimelineBoundary) -> String {
    let labels = boundary
        .serial_delta
        .iter()
        .map(|(service, delta)| {
            let omitted = (delta.omitted_bytes > 0)
                .then(|| format!("; +{} bytes", delta.omitted_bytes))
                .unwrap_or_default();
            format!(
                "{service}: {} [{} bytes; sha256 {}{}]",
                delta.excerpt, delta.bytes, delta.sha256, omitted
            )
        })
        .collect::<Vec<_>>();
    if labels.is_empty() {
        "none".to_owned()
    } else {
        labels.join(" · ")
    }
}

fn campaign_network_traffic_delta_label(boundary: &CampaignTimelineBoundary) -> String {
    let labels = boundary
        .network_traffic_delta
        .iter()
        .flat_map(|(service, networks)| {
            networks.iter().map(move |(network, delta)| {
                format!(
                    "{service}.{network}: tx {} rx {} drop {} dup {} corrupt {}",
                    delta.tx_frames,
                    delta.rx_frames,
                    delta.dropped,
                    delta.duplicated,
                    delta.corrupted,
                )
            })
        })
        .collect::<Vec<_>>();
    if labels.is_empty() {
        "none".to_owned()
    } else {
        labels.join(" · ")
    }
}

fn campaign_virtual_time_delta_label(boundary: &CampaignTimelineBoundary) -> String {
    if boundary.virtual_time_delta_ns.is_empty() {
        "none".to_owned()
    } else {
        boundary
            .virtual_time_delta_ns
            .iter()
            .map(|(service, clocks)| {
                format!(
                    "{service}: {} ns",
                    clocks
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

fn campaign_input_label(boundary: &CampaignTimelineBoundary) -> String {
    if boundary.input.sha256.is_empty() {
        return "unrecorded (legacy)".to_owned();
    }
    format!(
        "{} [{} bytes; sha256 {}{}]",
        boundary.input.excerpt,
        boundary.input.bytes,
        boundary.input.sha256,
        (boundary.input.omitted_bytes > 0)
            .then(|| format!("; +{} bytes", boundary.input.omitted_bytes))
            .unwrap_or_default(),
    )
}

fn campaign_uart_delivery_label(boundary: &CampaignTimelineBoundary) -> String {
    if !boundary.delivery.recorded {
        return "unrecorded (legacy)".to_owned();
    }
    format!(
        "accepted {} bytes; guest read {}; queued {} → {}{}",
        boundary.delivery.accepted_bytes,
        boundary.delivery.guest_read_bytes,
        boundary.delivery.pending_before,
        boundary.delivery.pending_after,
        if boundary.delivery.checkpoint.is_empty() {
            "; no marker barrier".to_owned()
        } else {
            format!("; waited for {}", boundary.delivery.checkpoint)
        },
    )
}

fn campaign_uart_barrier_label(boundary: &CampaignTimelineBoundary) -> String {
    if !boundary.barrier.recorded {
        return "unrecorded (legacy)".to_owned();
    }
    let response = &boundary.barrier.response;
    format!(
        "{} at round {}, +{}; {} [{} bytes; sha256 {}{}]",
        boundary.barrier.checkpoint,
        boundary.barrier.round,
        boundary.barrier.marker_offset,
        response.excerpt,
        response.bytes,
        response.sha256,
        (response.omitted_bytes > 0)
            .then(|| format!("; +{} bytes", response.omitted_bytes))
            .unwrap_or_default(),
    )
}

fn campaign_timeline_labels(run: &CampaignRun) -> Vec<[String; 17]> {
    run.timeline
        .iter()
        .map(|boundary| {
            let mut delta = Vec::new();
            if !boundary.new_markers.is_empty() {
                delta.push(format!("new markers: {}", boundary.new_markers.join(" ")));
            }
            if !boundary.changed_program_counters.is_empty() {
                delta.push(format!(
                    "changed PCs: {}",
                    boundary.changed_program_counters.join(", ")
                ));
            }
            if !boundary.changed_serial.is_empty() {
                delta.push(format!(
                    "changed serial: {}",
                    boundary.changed_serial.join(", ")
                ));
            }
            let delta = if delta.is_empty() {
                "none".to_owned()
            } else {
                delta.join("; ")
            };
            [
                run.index.to_string(),
                boundary.operation.clone(),
                if boundary.service.is_empty() {
                    "driver (legacy)".to_owned()
                } else {
                    boundary.service.clone()
                },
                campaign_input_label(boundary),
                campaign_uart_delivery_label(boundary),
                campaign_uart_barrier_label(boundary),
                boundary.round.to_string(),
                delta,
                boundary.markers.join(" "),
                instruction_location_labels(
                    &boundary.program_counters,
                    &boundary.instruction_locations,
                ),
                boundary
                    .actions
                    .iter()
                    .map(|action| format!("{} {}", action.kind, action.target))
                    .collect::<Vec<_>>()
                    .join(" · "),
                boundary
                    .serial_sha256
                    .iter()
                    .map(|(service, hash)| format!("{service}:{hash}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                campaign_serial_delta_label(boundary),
                campaign_network_traffic_delta_label(boundary),
                if boundary.changed_storage.is_empty() {
                    "none".to_owned()
                } else {
                    boundary.changed_storage.join(", ")
                },
                campaign_virtual_time_delta_label(boundary),
                boundary.state_sha256.clone(),
            ]
        })
        .collect()
}

fn campaign_posterior_label(run: &CampaignRun) -> String {
    run.guidance_evidence.as_ref().map_or_else(
        || "none".to_owned(),
        |evidence| {
            format!(
                "{}; {} yield(s), {} miss(es); mean {}‰ + {}‰ uncertainty",
                evidence.scope,
                evidence.successes,
                evidence.misses,
                evidence.mean_per_mille,
                evidence.uncertainty_per_mille,
            )
        },
    )
}

fn campaign_property_witness_label(run: &CampaignRun) -> String {
    if run.property_witnesses.is_empty() {
        "none".to_owned()
    } else {
        run.property_witnesses.join(", ")
    }
}

fn markdown_fence(value: &str) -> String {
    let width = value
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
        + 1;
    "`".repeat(width.max(3))
}

fn render_markdown(model: &ReportModel) -> String {
    let mut output = format!(
        "# Theseus failure report: {}\n\n**Status:** {}  \n**Kind:** {}\n\n## Reproduce\n\n```sh\n{}\n```\n",
        model.title, model.status, model.kind, model.command
    );
    if let Some(error) = &model.error {
        output.push_str("\n## Execution error\n\n");
        let fence = markdown_fence(error);
        output.push_str(&format!("{fence}\n{error}\n{fence}\n"));
    }
    if !model.checks.is_empty() {
        output.push_str(
            "\n## Checks\n\n| Name | Kind | Status | Detail |\n| --- | --- | --- | --- |\n",
        );
        for check in &model.checks {
            output.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                markdown_cell(&check.name),
                markdown_cell(&check.kind),
                markdown_cell(&check.status),
                markdown_cell(&check.detail)
            ));
        }
    }
    if let Some(coverage) = &model.coverage {
        output.push_str(&format!(
            "\n## {}\n\n{}\n",
            coverage.label, coverage.summary
        ));
    }
    if !model.nodes.is_empty() {
        output.push_str("\n## Timeline recipes\n\n");
        for node in &model.nodes {
            output.push_str(&format!(
                "- Timeline #{}: seed path `{}`; markers `{}`; dirty pages `{}`.\n",
                node.search_index,
                node.seed_path
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                if node.markers_hex.is_empty() {
                    "none"
                } else {
                    &node.markers_hex
                },
                node.dirty_pages
                    .map_or_else(|| "not captured".to_owned(), |pages| pages.to_string())
            ));
        }
    }
    if model
        .campaign_operations
        .iter()
        .any(|operation| !operation.service.is_empty())
    {
        output.push_str("\n## Operation targets\n\n| Operation | Service |\n| --- | --- |\n");
        for operation in &model.campaign_operations {
            if !operation.service.is_empty() {
                output.push_str(&format!(
                    "| {} | {} |\n",
                    markdown_cell(&operation.name),
                    markdown_cell(&operation.service)
                ));
            }
        }
    }
    if !model.campaign_runs.is_empty() {
        let has_posterior = model
            .campaign_runs
            .iter()
            .any(|run| run.guidance_evidence.is_some());
        let has_property_witnesses = model
            .campaign_runs
            .iter()
            .any(|run| !run.property_witnesses.is_empty());
        output.push_str("\n## Generated timelines\n\n");
        output.push_str("| Run | Operations | Candidates");
        if has_posterior {
            output.push_str(" | Posterior evidence");
        }
        if has_property_witnesses {
            output.push_str(" | Property witnesses");
        }
        output.push_str(" | Instruction locations | Status |\n| --- | --- | ---");
        if has_posterior {
            output.push_str(" | ---");
        }
        if has_property_witnesses {
            output.push_str(" | ---");
        }
        output.push_str(" | --- | --- |\n");
        for run in &model.campaign_runs {
            let candidates = if run.faults.is_empty() {
                run.fault.clone().unwrap_or_else(|| "none".to_owned())
            } else {
                run.faults.join(" + ")
            };
            output.push_str(&format!(
                "| {} | {} | {}",
                run.index,
                markdown_cell(&run.operations.join(" → ")),
                markdown_cell(&candidates),
            ));
            if has_posterior {
                output.push_str(&format!(
                    " | {}",
                    markdown_cell(&campaign_posterior_label(run)),
                ));
            }
            if has_property_witnesses {
                output.push_str(&format!(
                    " | {}",
                    markdown_cell(&campaign_property_witness_label(run)),
                ));
            }
            output.push_str(&format!(
                " | {} | {} |\n",
                markdown_cell(&campaign_instruction_location_labels(run)),
                markdown_cell(&run.status)
            ));
        }
    }
    let timeline = model
        .campaign_runs
        .iter()
        .flat_map(campaign_timeline_labels)
        .collect::<Vec<_>>();
    if !timeline.is_empty() {
        output.push_str("\n## Operation boundaries\n\nEach row is the paused checkpoint after one operation. Target is the service whose UART received it. UART input is an escaped, bounded copy of the delivered bytes; its hash covers the complete input in the locked replay plan. UART delivery records accepted bytes, guest FIFO reads, and queued bytes. UART barrier proves the named marker arrived after that input, with its post-input response hash and excerpt. The delta compares the checkpoint with the preceding one.\n\n| Run | Operation | Target | UART input | UART delivery | UART barrier | Round | Delta | Markers | Instruction locations | Applied actions | Serial SHA-256 | New serial output | Network traffic | Changed storage | Virtual time delta | State SHA-256 |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for [run, operation, target, input, delivery, barrier, round, delta, markers, locations, actions, serial, serial_output, traffic, storage, time, state] in
            timeline
        {
            output.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                markdown_cell(&run),
                markdown_cell(&operation),
                markdown_cell(&target),
                markdown_cell(&input),
                markdown_cell(&delivery),
                markdown_cell(&barrier),
                markdown_cell(&round),
                markdown_cell(&delta),
                markdown_cell(&markers),
                markdown_cell(&locations),
                markdown_cell(&actions),
                markdown_cell(&serial),
                markdown_cell(&serial_output),
                markdown_cell(&traffic),
                markdown_cell(&storage),
                markdown_cell(&time),
                markdown_cell(&state),
            ));
        }
    }
    if let Some(minimization) = &model.minimization {
        output.push_str(&format!(
            "\n## Event minimization\n\nOriginal: `{}`  \n1-minimal: `{}`\n",
            minimization.original_events_hex.join(" "),
            minimization.minimized_events_hex.join(" ")
        ));
    }
    if let Some(minimization) = &model.campaign_minimization {
        output.push_str(&format!(
            "\n## Campaign minimization\n\nProperty: `{}`  \nOperations: `{}` → `{}`  \nFaults: `{}` → `{}`\n",
            minimization.property,
            minimization.original_operations.join(" → "),
            minimization.minimized_operations.join(" → "),
            minimization.original_faults.join(" + "),
            minimization.minimized_faults.join(" + ")
        ));
    }
    if let Some(verification) = &model.replay_verification {
        output.push_str(&format!(
            "\n## Replay verification\n\n**{}:** {}\n",
            verification.status, verification.detail
        ));
    }
    if !model.faults.is_empty() {
        output.push_str("\n## Applied faults\n\n| Round | Kind | Detail | Barrier rounds |\n| --- | --- | --- | --- |\n");
        for fault in &model.faults {
            output.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                fault.round,
                markdown_cell(&fault.kind),
                markdown_cell(&fault.detail),
                fault
                    .barrier_rounds
                    .map_or_else(|| "—".to_owned(), |rounds| rounds.to_string())
            ));
        }
    }
    if !model.logs.is_empty() {
        output.push_str("\n## Logs\n");
        for log in &model.logs {
            let fence = markdown_fence(&log.text);
            output.push_str(&format!(
                "\n### {}\n\n{fence}\n{}\n{fence}\n",
                log.label, log.text
            ));
        }
    }
    output
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn render_junit(model: &ReportModel) -> String {
    let structural_failure = model.status != "passed"
        && model.error.is_none()
        && model
            .checks
            .iter()
            .all(|check| check.status == "passed" || check.status == "skipped");
    let failures = model
        .checks
        .iter()
        .filter(|check| check.status != "passed" && check.status != "skipped")
        .count()
        + usize::from(structural_failure);
    let errors = usize::from(model.error.is_some());
    let tests = model.checks.len() + usize::from(model.error.is_some() || structural_failure);
    let tests = tests.max(1);
    let mut output = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"{}\" tests=\"{tests}\" failures=\"{failures}\" errors=\"{errors}\">\n",
        xml_escape(&model.title)
    );
    for check in &model.checks {
        output.push_str(&format!(
            "  <testcase classname=\"{}\" name=\"{}\">",
            xml_escape(if check.kind.is_empty() {
                "theseus"
            } else {
                &check.kind
            }),
            xml_escape(&check.name)
        ));
        match check.status.as_str() {
            "passed" => {}
            "skipped" => output.push_str("<skipped/>"),
            _ => output.push_str(&format!(
                "<failure message=\"{}\">{}</failure>",
                xml_escape(&check.detail),
                xml_escape(&check.detail)
            )),
        }
        output.push_str("</testcase>\n");
    }
    if let Some(error) = &model.error {
        output.push_str(&format!(
            "  <testcase classname=\"theseus\" name=\"execution\"><error message=\"{}\">{}</error></testcase>\n",
            xml_escape(error),
            xml_escape(error)
        ));
    } else if structural_failure {
        output.push_str(&format!(
            "  <testcase classname=\"theseus\" name=\"result\"><failure message=\"{}\">The locked result directory reports {}.</failure></testcase>\n",
            xml_escape(&model.status),
            xml_escape(&model.status)
        ));
    } else if model.checks.is_empty() {
        output.push_str("  <testcase classname=\"theseus\" name=\"result\"/>\n");
    }
    output.push_str(&format!(
        "  <system-out>{}</system-out>\n</testsuite>\n",
        xml_escape(&format!("{}\n{}", model.command_label, model.command))
    ));
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_json(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn renders_a_safe_single_timeline_report() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("result.json"),
            r#"{"format":"theseus-result-v1","status":"failed","error":null,"checks":[{"name":"no-panic","kind":"serial_not_contains","status":"passed","detail":"ok"}]}"#,
        );
        fs::write(
            directory.path().join("serial.log"),
            b"<img src=x onerror=alert(1)>",
        )
        .unwrap();
        let output = directory.path().join("report");
        let index = report(directory.path(), &output).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Timeline replay"));
        assert!(html.contains("theseus replay"));
        assert!(html.contains("\\u003cimg"));
        assert!(!html.contains("<img src=x"));
    }

    #[test]
    fn renders_portable_markdown_json_and_junit_reports() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("result.json"),
            r#"{"format":"theseus-result-v1","status":"failed","error":null,"checks":[{"name":"no panic","kind":"serial_not_contains","status":"failed","detail":"found <panic>"}]}"#,
        );
        fs::write(directory.path().join("serial.log"), b"guest ``` panic\n").unwrap();

        let markdown = report_text(directory.path(), ReportFormat::Markdown).unwrap();
        assert!(markdown.contains("# Theseus failure report: Timeline replay"));
        assert!(markdown.contains("theseus replay"));
        assert!(markdown.contains("found <panic>"));
        assert!(markdown.contains("````"));

        let json = report_text(directory.path(), ReportFormat::Json).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json["format"], "theseus-report-v1");
        assert_eq!(json["checks"][0]["status"], "failed");

        let junit = report_text(directory.path(), ReportFormat::Junit).unwrap();
        assert!(junit.contains("tests=\"1\" failures=\"1\" errors=\"0\""));
        assert!(junit.contains("&lt;panic&gt;"));
        assert!(junit.contains("<failure"));
    }

    #[test]
    fn writes_a_new_portable_report_file() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("result.json"),
            r#"{"format":"theseus-result-v1","status":"passed","error":null,"checks":[]}"#,
        );
        let output = directory.path().join("nested/report.json");
        let path = report_file(directory.path(), ReportFormat::Json, &output).unwrap();
        assert_eq!(path, output);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(path).unwrap()).unwrap()
                ["format"],
            "theseus-report-v1"
        );
    }

    #[test]
    fn renders_topology_faults_and_service_logs() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","compose":"/tmp/compose.yaml"}"#,
        );
        let service = directory.path().join("services/api");
        fs::create_dir_all(&service).unwrap();
        write_json(
            &service.join("result.json"),
            r#"{"status":"passed","checks":[{"name":"guest_exit","status":"passed","detail":"ok"}],"faults":[{"round":2,"kind":"restart","detail":"restarted","barrier_rounds":3}]}"#,
        );
        write_json(
            &directory.path().join("minimization.json"),
            r#"{"property":"consistent_read","original_operations":["write","retry","read"],"minimized_operations":["write","read"],"original_faults":["backplane:partition@write","backplane:heal@read"],"minimized_faults":["backplane:partition@write"],"operation_attempts":4,"fault_attempts":2}"#,
        );
        fs::write(service.join("serial.log"), b"ready\n").unwrap();
        let index = report(directory.path(), directory.path().join("report")).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Topology replay"));
        assert!(html.contains("restart"));
        assert!(html.contains("Barrier rounds"));
        assert!(html.contains("\"barrier_rounds\":3"));
        assert!(html.contains("api: serial.log"));
        assert!(html.contains("Campaign minimization"));
        assert!(html.contains("1-minimal faults"));
        assert!(html.contains("backplane:partition@write"));
        assert!(html.contains("Operation replays"));
    }

    #[test]
    fn renders_an_autonomous_compose_campaign() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","campaign":{"operations":[{"name":"write","max_uses":1},{"name":"close","requires":["write"],"requires_markers":["written"],"requires_serial":{"json":{"fields":{"/passed":false}}},"requires_serial_all":[{"service":"auditor","json":{"fields":{"/event":"ready"}}}],"requires_serial_joins":[{"endpoints":[{"pointer":"/request_id","json":{"fields":{"/event":"write"}}},{"service":"auditor","pointer":"/request_id","json":{"fields":{"/event":"audit"}}}]}],"requires_serial_evidence":{"all":[{"guard":{"contains":"THES:M:written"}},{"join":{"endpoints":[{"pointer":"/request_id","json":{"fields":{"/event":"write"}}},{"service":"auditor","pointer":"/request_id","json":{"fields":{"/event":"audit"}}}]}}]},"excludes_serial_evidence":{"guard":{"contains":"THES:ASSERT:panic"}},"max_uses":1},{"name":"read","requires":["write"],"excludes":["close"],"excludes_markers":["closed"]}]}}"#,
        );
        write_json(
            &directory.path().join("campaign-result.json"),
            r#"{"format":"theseus-compose-campaign-result-v1","status":"failed","driver":"api","guidance":"posterior","checkpoint_nodes":4,"checkpoint_reuses":7,"generated_candidates":12,"marker_guard_rejections":2,"serial_guard_rejections":1,"unique_topology_states":3,"search":{"checkpoint":{"root_captures":1,"prefix_captures":3,"checkpoint_nodes":4,"prefix_reuses":7,"prefix_restores":3,"leaf_restores":1,"topology_restores":4,"avoided_prefix_recomputations":7,"retained_memory_bytes":1048576,"shared_cow_restore_bytes":4194304,"private_dirty_pages":12,"snapshot_file_bytes":0},"guidance_observations":1,"guidance_sha256":"ledger-hash"},"replay_verification":{"status":"passed","detail":"1 recorded campaign timelines reproduced"},"runs":[{"index":0,"operations":["write","read"],"faults":["backplane:partition@write","backplane:heal@read"],"selection":"extends 1-operation prefix with 2 new marker(s) and new topology state","guidance_ledger":{"observations":1,"sha256":"ledger-hash"},"guidance_evidence":{"action":"read","context":["write"],"scope":"exact context","successes":2,"misses":1,"mean_per_mille":600,"uncertainty_per_mille":100,"score":44800},"timeline":[{"operation":"write","round":7,"markers":["42"],"new_markers":["42"],"changed_program_counters":["api"],"changed_serial":["api"],"program_counters":{"api":["0x7000"]},"instruction_locations":{"api":[{"address":"0x7000","symbol":"write","offset":0}]},"actions":[{"kind":"partition","target":"network:backplane"}],"serial_sha256":{"api":"abc123"},"serial_delta":{"api":{"bytes":16,"sha256":"write-hash","excerpt":"write\\ncomplete\\n","omitted_bytes":0}},"network_traffic_delta":{"api":{"backplane":{"tx_frames":2,"rx_frames":1,"dropped":1,"duplicated":0,"corrupted":0}}},"changed_storage":["api:data"],"virtual_time_delta_ns":{"api":[1000]},"state_sha256":"boundary-write"},{"operation":"read","service":"worker","input":{"bytes":5,"sha256":"input-hash","excerpt":"read\\n","omitted_bytes":0},"delivery":{"recorded":true,"accepted_bytes":5,"pending_before":1,"pending_after":0,"guest_read_bytes":6,"checkpoint":"THES:M:read"},"barrier":{"recorded":true,"checkpoint":"THES:M:read","marker_offset":6,"response":{"bytes":17,"sha256":"barrier-hash","excerpt":"reply THES:M:read","omitted_bytes":0}},"round":9,"markers":["42","a1"],"new_markers":["a1"],"changed_program_counters":["api"],"changed_serial":["api"],"program_counters":{"api":["0x8000"]},"instruction_locations":{"api":[{"address":"0x8000","symbol":"checkpoint","offset":7,"source":{"file":"kernel/init/main.c","line":812,"column":4}}]},"serial_sha256":{"api":"def456"},"serial_delta":{"api":{"bytes":11,"sha256":"read-hash","excerpt":"read\\nready\\n","omitted_bytes":0}},"network_traffic_delta":{"api":{"backplane":{"tx_frames":0,"rx_frames":2,"dropped":0,"duplicated":1,"corrupted":0}}},"changed_storage":[],"virtual_time_delta_ns":{"api":[2000,3000]},"state_sha256":"boundary-read"}],"program_counters":{"api":["0x8000"]},"instruction_locations":{"api":[{"address":"0x8000","symbol":"checkpoint","offset":7,"source":{"file":"kernel/init/main.c","line":812,"column":4}}]},"state_novel":true,"actions":[{"kind":"partition","target":"network:backplane"}],"status":"failed","novelty":["42","a1"]}],"properties":[{"name":"consistent_read","kind":"always","status":"failed","detail":"0 of 1 retained timelines contained \"pass\""}]}"#,
        );
        let index = report(directory.path(), directory.path().join("report")).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Autonomous Compose campaign"));
        assert!(html.contains("Generated timelines"));
        assert!(html.contains("backplane:partition@write"));
        assert!(html.contains("backplane:heal@read"));
        assert!(html.contains("Candidates"));
        assert!(html.contains("Applied actions"));
        assert!(html.contains("network:backplane"));
        assert!(html.contains("consistent_read"));
        assert!(html.contains(
            "1 root captures, 4 reusable checkpoint nodes, 3 prefix captures, 7 prefix reuses (7 avoided recomputations), 4 topology restores (3 prefix materializations + 1 leaf replays); 1048576 retained immutable bytes, 4194304 logical COW-mapped restore bytes, 12 dirty pages at capture barriers, 0 snapshot-file bytes; guidance ledger: 1 observations, sha256 ledger-hash"
        ));
        assert!(html.contains(
            "1 of 12 deterministic candidates selected by posterior coverage and action-yield guidance"
        ));
        assert!(html.contains("2 marker-guard leaves and 1 serial-guard leaves skipped"));
        assert!(html.contains("3 unique topology states"));
        assert!(
            html.contains("extends 1-operation prefix with 2 new marker(s) and new topology state")
        );
        assert!(html.contains("Topology state"));
        assert!(html.contains("Guidance ledger"));
        assert!(
            html.contains("\"guidance_ledger\":{\"observations\":1,\"sha256\":\"ledger-hash\"}")
        );
        assert!(html.contains("Instruction locations"));
        assert!(html.contains("Operation boundaries"));
        assert!(html.contains("Target"));
        assert!(html.contains("UART input"));
        assert!(html.contains("UART delivery"));
        assert!(html.contains("UART barrier"));
        assert!(html.contains("worker"));
        assert!(html.contains("driver (legacy)"));
        assert!(html.contains("input-hash"));
        assert!(html.contains("\"guest_read_bytes\":6"));
        assert!(html.contains("b.delivery.guest_read_bytes"));
        assert!(html.contains("Serial SHA-256"));
        assert!(html.contains("changed PCs"));
        assert!(html.contains("New serial output"));
        assert!(html.contains("Property witnesses"));
        assert!(html.contains("Changed storage"));
        assert!(html.contains("Virtual time delta"));
        assert!(html.contains("State SHA-256"));
        assert!(html.contains("write\\\\ncomplete\\\\n"));
        assert!(html.contains("abc123"));
        assert!(html.contains("Posterior evidence"));
        assert!(html.contains("guidance_evidence"));
        assert!(html.contains("p.mean_per_mille"));
        assert!(html.contains("0x8000"));
        assert!(html.contains("checkpoint"));
        assert!(html.contains("\"file\":\"kernel/init/main.c\""));
        assert!(html.contains("l.source.file"));
        assert!(html.contains("\"offset\":7"));
        let markdown = report_text(directory.path(), ReportFormat::Markdown).unwrap();
        assert!(markdown.contains("0x8000 → checkpoint +0x7 · kernel/init/main.c:812:4"));
        assert!(markdown.contains("## Operation boundaries"));
        assert!(markdown.contains("UART input"));
        assert!(markdown.contains("UART delivery"));
        assert!(markdown.contains("UART barrier"));
        assert!(markdown.contains("unrecorded (legacy)"));
        assert!(markdown.contains("read\\n [5 bytes; sha256 input-hash]"));
        assert!(markdown
            .contains("accepted 5 bytes; guest read 6; queued 1 → 0; waited for THES:M:read"));
        assert!(markdown.contains(
            "THES:M:read at round 0, +6; reply THES:M:read [17 bytes; sha256 barrier-hash]"
        ));
        assert!(markdown.contains(
            "write | driver (legacy) | unrecorded (legacy) | unrecorded (legacy) | unrecorded (legacy) | 7 | new markers: 42; changed PCs: api; changed serial: api"
        ));
        assert!(markdown.contains("new markers: 42; changed PCs: api; changed serial: api"));
        assert!(markdown.contains("api: write\\ncomplete\\n [16 bytes; sha256 write-hash]"));
        assert!(markdown.contains("api:data"));
        assert!(markdown.contains("api: 1000 ns"));
        assert!(markdown.contains("boundary-write"));
        assert!(markdown
            .contains("exact context; 2 yield(s), 1 miss(es); mean 600‰ + 100‰ uncertainty"));
        assert!(html.contains("Operation model"));
        assert!(html.contains("(c.select||'latest')"));
        assert!(html.contains("JSON.stringify(c.json||c.workflow||{sequence:c.sequence})"));
        assert!(html.contains("Requires earlier"));
        assert!(html.contains("Excludes earlier"));
        assert!(html.contains("Requires observed marker"));
        assert!(html.contains("Requires serial predicate"));
        assert!(html.contains("Requires all serial guards"));
        assert!(html.contains("Requires JSON joins"));
        assert!(html.contains("Excludes JSON joins"));
        assert!(html.contains("Requires serial evidence"));
        assert!(html.contains("Excludes serial evidence"));
        assert!(html.contains("\"requires_serial_joins\""));
        assert!(html.contains("\"requires_serial_evidence\""));
        assert!(html.contains("{\"json\":{\"fields\":{\"/passed\":false}}}"));
        assert!(html.contains("Excludes observed marker"));
        assert!(html.contains("Maximum uses"));
        assert!(html.contains("unbounded"));
        assert!(html.contains("Replay verification"));
        assert!(html.contains("1 recorded campaign timelines reproduced"));
    }

    #[test]
    fn renders_property_witnesses_for_property_directed_campaigns() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","campaign":{"operations":[{"name":"compact","service":"worker"}]}}"#,
        );
        write_json(
            &directory.path().join("campaign-result.json"),
            r#"{"format":"theseus-compose-campaign-result-v1","status":"passed","driver":"api","guidance":"property","runs":[{"index":0,"operations":["read"],"property_witnesses":["stale_read_is_reachable"],"status":"passed"}],"properties":[]}"#,
        );

        let markdown = report_text(directory.path(), ReportFormat::Markdown).unwrap();
        assert!(markdown.contains("declared-property and coverage guidance"));
        assert!(markdown.contains("Property witnesses"));
        assert!(markdown.contains("stale_read_is_reachable"));
        assert!(markdown.contains("## Operation targets"));
        assert!(markdown.contains("compact | worker"));

        let index = report(directory.path(), directory.path().join("report")).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Property witnesses"));
        assert!(html.contains("stale_read_is_reachable"));
        assert!(html.contains("Operation targets"));
    }

    #[test]
    fn renders_exploration_tree_and_coverage_proxy() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("explore-plan.json"),
            r#"{"manifest":"/tmp/theseus.toml"}"#,
        );
        write_json(
            &directory.path().join("result.json"),
            r#"{"format":"theseus-exploration-result-v1","status":"failed","error":null,"checks":[{"name":"every timeline completed","kind":"marker_seen","status":"failed","detail":"missing ff"}],"nodes":[{"search_index":0,"id":0,"parent":null,"depth":0,"seed":1,"seed_path":[1],"entropy_probe_hex":"aa","markers_hex":"42","dirty_pages":3,"serial_log":"serial/1.log"},{"search_index":1,"id":1,"parent":0,"depth":1,"seed":2,"seed_path":[1,2],"entropy_probe_hex":"bb","markers_hex":"43","dirty_pages":5,"serial_log":"serial/2.log"}],"minimization":{"original_events_hex":["01","02","03"],"minimized_events_hex":["02"]}}"#,
        );
        fs::create_dir(directory.path().join("serial")).unwrap();
        fs::write(directory.path().join("serial/1.log"), b"root ready\n").unwrap();
        fs::write(directory.path().join("serial/2.log"), b"child ready\n").unwrap();
        let index = report(directory.path(), directory.path().join("report")).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Timeline tree"));
        assert!(html.contains("Dirty-page footprint"));
        assert!(html.contains("every timeline completed"));
        assert!(html.contains("exploration-rerun"));
        assert!(html.contains("--seed-path"));
        assert!(html.contains("--minimize"));
        assert!(html.contains("--snapshot"));
        assert!(html.contains("Event minimization"));
        assert!(html.contains("Timeline #1 serial log"));
        assert!(html.contains("child ready"));
    }
}
