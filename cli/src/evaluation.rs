use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum EvaluationError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        detail: String,
    },
    Invalid(String),
}

impl std::fmt::Display for EvaluationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, detail } => {
                write!(formatter, "cannot parse {}: {detail}", path.display())
            }
            Self::Invalid(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for EvaluationError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluationSpec {
    version: u8,
    name: String,
    #[serde(default)]
    baseline: Option<Baseline>,
    workloads: Vec<WorkloadSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    method: String,
    runs: usize,
    counterexamples: usize,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkloadSpec {
    name: String,
    bundle: String,
    expected_status: String,
    #[serde(default)]
    properties: Vec<ExpectedProperty>,
    /// A manually observed host-time note. It is deliberately not used for
    /// pass/fail or replay evidence.
    #[serde(default)]
    investigation_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedProperty {
    name: String,
    status: String,
}

#[derive(Deserialize)]
struct CampaignResult {
    format: String,
    status: String,
    #[serde(default)]
    generated_candidates: usize,
    #[serde(default)]
    unique_topology_states: usize,
    #[serde(default)]
    unique_instruction_locations: usize,
    #[serde(default)]
    search: SearchEvidence,
    #[serde(default)]
    replay_verification: Option<ReplayVerification>,
    #[serde(default)]
    runs: Vec<CampaignRun>,
    #[serde(default)]
    properties: Vec<CampaignProperty>,
}

#[derive(Default, Deserialize)]
struct SearchEvidence {
    #[serde(default)]
    checkpoint: CheckpointEvidence,
}

#[derive(Default, Deserialize)]
struct CheckpointEvidence {
    #[serde(default)]
    root_captures: usize,
    #[serde(default)]
    prefix_captures: usize,
    #[serde(default)]
    checkpoint_nodes: usize,
    #[serde(default)]
    prefix_reuses: usize,
    #[serde(default)]
    avoided_prefix_recomputations: usize,
}

#[derive(Deserialize)]
struct ReplayVerification {
    status: String,
}

#[derive(Deserialize)]
struct CampaignRun {
    #[serde(default)]
    timeline: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct CampaignProperty {
    name: String,
    status: String,
}

#[derive(Default, Deserialize)]
struct Minimization {
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

#[derive(Serialize)]
pub struct EvaluationSummary {
    pub format: &'static str,
    pub name: String,
    pub status: &'static str,
    pub workloads: Vec<EvaluatedWorkload>,
    pub replay: ReplayMetric,
    pub search: SearchMetric,
    pub reduction: ReductionMetric,
    pub investigation: InvestigationMetric,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conventional_baseline: Option<BaselineMetric>,
}

#[derive(Serialize)]
pub struct EvaluatedWorkload {
    pub name: String,
    pub bundle: String,
    pub status: String,
    pub expected_status: String,
    pub replay_verified: bool,
    pub expected_properties: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub investigation_seconds: Option<u64>,
}

#[derive(Default, Serialize)]
pub struct ReplayMetric {
    pub verified: usize,
    pub total: usize,
}

#[derive(Default, Serialize)]
pub struct SearchMetric {
    pub generated_candidates: usize,
    pub retained_runs: usize,
    pub unique_topology_states: usize,
    pub unique_instruction_locations: usize,
    pub root_captures: usize,
    pub prefix_captures: usize,
    pub checkpoint_nodes: usize,
    pub prefix_reuses: usize,
    pub avoided_prefix_recomputations: usize,
}

#[derive(Default, Serialize)]
pub struct ReductionMetric {
    pub original_operations: usize,
    pub minimized_operations: usize,
    pub original_faults: usize,
    pub minimized_faults: usize,
    pub operation_replays: usize,
    pub fault_replays: usize,
}

#[derive(Default, Serialize)]
pub struct InvestigationMetric {
    /// A deterministic amount of retained evidence to inspect, not host time.
    pub retained_operation_boundaries: usize,
    /// Optional human-recorded times; excluded from all evaluation verdicts.
    pub manually_reported_seconds: u64,
}
#[derive(Serialize)]
pub struct BaselineMetric {
    pub method: String,
    pub runs: usize,
    pub counterexamples: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl EvaluationSummary {
    pub fn markdown(&self) -> String {
        let mut output = format!(
            "# Theseus public evaluation: {}\n\nStatus: **{}**\n\n",
            self.name, self.status
        );
        output.push_str("## Replay and search evidence\n\n");
        output.push_str(&format!(
            "- Replay verification: {}/{} bundles\n- Generated candidates: {}\n- Retained runs: {}\n- Unique topology states: {}\n- Unique instruction locations: {}\n- Checkpoint work: {} root captures, {} prefix captures, {} nodes, {} prefix reuses, {} avoided recomputations\n",
            self.replay.verified,
            self.replay.total,
            self.search.generated_candidates,
            self.search.retained_runs,
            self.search.unique_topology_states,
            self.search.unique_instruction_locations,
            self.search.root_captures,
            self.search.prefix_captures,
            self.search.checkpoint_nodes,
            self.search.prefix_reuses,
            self.search.avoided_prefix_recomputations,
        ));
        output.push_str(&format!(
            "\n## Reduction and investigation\n\n- Reduction: {} → {} operations; {} → {} faults; {} operation replays; {} fault replays\n- Retained operation boundaries: {}\n- Manually reported investigation seconds: {} (informational; never a replay verdict)\n",
            self.reduction.original_operations,
            self.reduction.minimized_operations,
            self.reduction.original_faults,
            self.reduction.minimized_faults,
            self.reduction.operation_replays,
            self.reduction.fault_replays,
            self.investigation.retained_operation_boundaries,
            self.investigation.manually_reported_seconds,
        ));
        if let Some(baseline) = &self.conventional_baseline {
            output.push_str(&format!(
                "\n## Conventional baseline\n\n- Method: {}\n- Runs: {}\n- Counterexamples: {}\n{}\n",
                baseline.method,
                baseline.runs,
                baseline.counterexamples,
                baseline.note,
            ));
        }
        output.push_str("\n## Workloads\n\n| Workload | Result | Replay | Expected properties |\n| --- | --- | --- | --- |\n");
        for workload in &self.workloads {
            output.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                workload.name,
                workload.status,
                yes_no(workload.replay_verified),
                yes_no(workload.expected_properties),
            ));
        }
        output
    }
}

pub fn evaluate(path: impl AsRef<Path>) -> Result<EvaluationSummary, EvaluationError> {
    let path = path.as_ref();
    let root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let root = fs::canonicalize(root).map_err(|source| EvaluationError::Read {
        path: root.to_path_buf(),
        source,
    })?;
    let spec: EvaluationSpec = read_toml(path)?;
    if spec.version != 1 {
        return Err(EvaluationError::Invalid(format!(
            "evaluation version must be 1, got {}",
            spec.version
        )));
    }
    if spec.name.is_empty() || spec.workloads.is_empty() {
        return Err(EvaluationError::Invalid(
            "evaluation needs a name and at least one workload".to_owned(),
        ));
    }

    let mut workloads = Vec::with_capacity(spec.workloads.len());
    let mut replay = ReplayMetric {
        verified: 0,
        total: 0,
    };
    let mut search = SearchMetric::default();
    let mut reduction = ReductionMetric::default();
    let mut investigation = InvestigationMetric::default();
    let mut passed = true;
    for workload in spec.workloads {
        let bundle = resolve_bundle(&root, &workload.bundle)?;
        let result: CampaignResult = read_json(&bundle.join("campaign-result.json"))?;
        if result.format != "theseus-compose-campaign-result-v1" {
            return Err(EvaluationError::Invalid(format!(
                "{} is not a Compose campaign result",
                bundle.display()
            )));
        }
        let replay_verified = result
            .replay_verification
            .as_ref()
            .is_some_and(|verification| verification.status == "passed");
        let expected_properties = workload.properties.iter().all(|expected| {
            result
                .properties
                .iter()
                .any(|actual| actual.name == expected.name && actual.status == expected.status)
        });
        let workload_passed = result.status == workload.expected_status && expected_properties;
        passed &= workload_passed && replay_verified;
        replay.total += 1;
        replay.verified += usize::from(replay_verified);
        search.generated_candidates += result.generated_candidates;
        search.retained_runs += result.runs.len();
        search.unique_topology_states += result.unique_topology_states;
        search.unique_instruction_locations += result.unique_instruction_locations;
        search.root_captures += result.search.checkpoint.root_captures;
        search.prefix_captures += result.search.checkpoint.prefix_captures;
        search.checkpoint_nodes += result.search.checkpoint.checkpoint_nodes;
        search.prefix_reuses += result.search.checkpoint.prefix_reuses;
        search.avoided_prefix_recomputations +=
            result.search.checkpoint.avoided_prefix_recomputations;
        investigation.retained_operation_boundaries += result
            .runs
            .iter()
            .map(|run| run.timeline.len())
            .sum::<usize>();
        investigation.manually_reported_seconds += workload.investigation_seconds.unwrap_or(0);
        let minimized: Minimization =
            read_json_optional(&bundle.join("minimization.json"))?.unwrap_or_default();
        reduction.original_operations += minimized.original_operations.len();
        reduction.minimized_operations += minimized.minimized_operations.len();
        reduction.original_faults += minimized.original_faults.len();
        reduction.minimized_faults += minimized.minimized_faults.len();
        reduction.operation_replays += minimized.operation_attempts;
        reduction.fault_replays += minimized.fault_attempts;
        workloads.push(EvaluatedWorkload {
            name: workload.name,
            bundle: workload.bundle,
            status: result.status,
            expected_status: workload.expected_status,
            replay_verified,
            expected_properties,
            investigation_seconds: workload.investigation_seconds,
        });
    }
    Ok(EvaluationSummary {
        format: "theseus-public-evaluation-v1",
        name: spec.name,
        status: if passed { "passed" } else { "failed" },
        workloads,
        replay,
        search,
        reduction,
        investigation,
        conventional_baseline: spec.baseline.map(|baseline| BaselineMetric {
            method: baseline.method,
            runs: baseline.runs,
            counterexamples: baseline.counterexamples,
            note: baseline.note,
        }),
    })
}

fn read_toml(path: &Path) -> Result<EvaluationSpec, EvaluationError> {
    let contents = fs::read_to_string(path).map_err(|source| EvaluationError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&contents).map_err(|error| EvaluationError::Parse {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, EvaluationError> {
    let contents = fs::read(path).map_err(|source| EvaluationError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&contents).map_err(|error| EvaluationError::Parse {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })
}

fn read_json_optional<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<Option<T>, EvaluationError> {
    if !path.is_file() {
        return Ok(None);
    }
    read_json(path).map(Some)
}

fn resolve_bundle(root: &Path, bundle: &str) -> Result<PathBuf, EvaluationError> {
    let bundle = root.join(bundle);
    let bundle = fs::canonicalize(&bundle).map_err(|source| EvaluationError::Read {
        path: bundle,
        source,
    })?;
    if !bundle.starts_with(root) {
        return Err(EvaluationError::Invalid(format!(
            "evaluation bundle escapes its directory: {}",
            bundle.display()
        )));
    }
    Ok(bundle)
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_locked_campaign_metrics_without_a_runner() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("bundle")).unwrap();
        fs::write(
            directory.path().join("theseus-evaluation.toml"),
            r#"version = 1
name = "replicated counter"
[baseline]
method = "fixed chaos schedule"
runs = 10
counterexamples = 0
note = "informational only"
[[workloads]]
name = "stale read"
bundle = "bundle"
expected_status = "failed"
investigation_seconds = 12
[[workloads.properties]]
name = "consistent_read"
status = "failed"
"#,
        )
        .unwrap();
        fs::write(directory.path().join("bundle/campaign-result.json"), r#"{"format":"theseus-compose-campaign-result-v1","status":"failed","generated_candidates":8,"unique_topology_states":3,"unique_instruction_locations":5,"search":{"checkpoint":{"root_captures":1,"prefix_captures":2,"checkpoint_nodes":3,"prefix_reuses":4,"avoided_prefix_recomputations":5}},"replay_verification":{"status":"passed"},"runs":[{"timeline":[{},{}]}],"properties":[{"name":"consistent_read","status":"failed"}]}"#).unwrap();
        fs::write(directory.path().join("bundle/minimization.json"), r#"{"original_operations":["write","read"],"minimized_operations":["write"],"original_faults":["partition"],"minimized_faults":[],"operation_attempts":3,"fault_attempts":2}"#).unwrap();
        let summary = evaluate(directory.path().join("theseus-evaluation.toml")).unwrap();
        assert_eq!(summary.status, "passed");
        assert_eq!(summary.replay.verified, 1);
        assert_eq!(summary.search.unique_instruction_locations, 5);
        assert_eq!(summary.reduction.minimized_operations, 1);
        assert_eq!(summary.investigation.retained_operation_boundaries, 2);
        assert!(summary.markdown().contains("fixed chaos schedule"));
    }

    #[test]
    fn marks_a_missing_replay_witness_as_a_failed_evaluation() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("bundle")).unwrap();
        fs::write(
            directory.path().join("theseus-evaluation.toml"),
            r#"version = 1
name = "missing replay witness"
[[workloads]]
name = "counter"
bundle = "bundle"
expected_status = "failed"
"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("bundle/campaign-result.json"),
            r#"{"format":"theseus-compose-campaign-result-v1","status":"failed"}"#,
        )
        .unwrap();
        let summary = evaluate(directory.path().join("theseus-evaluation.toml")).unwrap();
        assert_eq!(summary.status, "failed");
        assert_eq!(summary.replay.verified, 0);
    }
}
