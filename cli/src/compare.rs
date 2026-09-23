use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

type Coverage = std::collections::BTreeMap<String, Vec<Value>>;

#[derive(Debug)]
pub enum CompareError {
    Read(std::io::Error),
    Parse(serde_json::Error),
    Invalid(String),
}

impl std::fmt::Display for CompareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "cannot read campaign result: {error}"),
            Self::Parse(error) => write!(formatter, "cannot parse campaign result: {error}"),
            Self::Invalid(reason) => write!(formatter, "invalid comparison input: {reason}"),
        }
    }
}

impl std::error::Error for CompareError {}

impl From<std::io::Error> for CompareError {
    fn from(error: std::io::Error) -> Self {
        Self::Read(error)
    }
}

impl From<serde_json::Error> for CompareError {
    fn from(error: serde_json::Error) -> Self {
        Self::Parse(error)
    }
}

#[derive(Deserialize)]
struct ResultFile {
    #[serde(default)]
    runs: Vec<Run>,
    #[serde(default)]
    properties: Vec<Property>,
    /// The locked fork decision a counterfactual re-execution carried, so
    /// the forked future can be paired with the retained base run it forked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    counterfactual: Option<CounterfactualProvenance>,
}

#[derive(Clone, Deserialize)]
struct CounterfactualProvenance {
    #[serde(default)]
    run: usize,
    #[serde(default)]
    fault: String,
    #[serde(default)]
    replace: String,
}
#[derive(Deserialize, PartialEq, Serialize)]
struct Property {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    detail: String,
}

#[derive(Deserialize)]
struct Run {
    index: usize,
    #[serde(default)]
    operations: Vec<String>,
    #[serde(default)]
    decision_trace: Vec<String>,
    #[serde(default)]
    thread_schedule_prefixes: Vec<Vec<u8>>,
    #[serde(default)]
    faults: Vec<String>,
    #[serde(default)]
    actions: Vec<Value>,
    #[serde(default)]
    property_witnesses: Vec<String>,
    #[serde(default)]
    program_counters: Coverage,
    #[serde(default)]
    instruction_locations: Coverage,
    #[serde(default)]
    instruction_novelty: Vec<String>,
    #[serde(default)]
    checkpoint_pc_novelty: Vec<String>,
    #[serde(default)]
    application_blocks: Coverage,
    #[serde(default)]
    application_block_novelty: Vec<String>,
    #[serde(default)]
    thread_scheduling: Coverage,
    #[serde(default)]
    thread_synchronization: Coverage,
    #[serde(default)]
    structured_choices: Coverage,
    #[serde(default)]
    execution_ledgers: Coverage,
    #[serde(default)]
    machine_execution_ledgers: Value,
    #[serde(default)]
    machine_execution_traces: Value,
    #[serde(default)]
    state_sha256: String,
    #[serde(default)]
    timeline: Vec<Boundary>,
}
#[derive(Deserialize)]
struct Boundary {
    #[serde(default)]
    id: String,
    /// `<vtime_ns>@<input_sha256>` moment address; absent in results
    /// recorded before moments existed.
    #[serde(default)]
    moment: String,
    #[serde(default)]
    operation: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    state_sha256: String,
    #[serde(default)]
    serial_sha256: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    markers: Vec<String>,
    #[serde(default)]
    actions: Vec<Value>,
    #[serde(default)]
    program_counters: Coverage,
    #[serde(default)]
    instruction_locations: Coverage,
    #[serde(default)]
    application_blocks: Coverage,
    #[serde(default)]
    thread_scheduling: Coverage,
    #[serde(default)]
    thread_synchronization: Coverage,
    #[serde(default)]
    structured_choices: Coverage,
    #[serde(default)]
    execution_ledgers: Coverage,
    #[serde(default)]
    machine_execution_ledgers: Value,
}

#[derive(Serialize)]
pub struct CampaignComparison {
    pub format: &'static str,
    pub status: &'static str,
    pub left_runs: usize,
    pub right_runs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub divergence: Option<CampaignDivergence>,
}
#[derive(Debug, Serialize)]
pub struct CampaignDivergence {
    pub run: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<usize>,
    pub reason: String,
    pub left: String,
    pub right: String,
    /// Both sides' moment addresses at the diverging boundary,
    /// `<vtime_ns>@<input_sha256>`, so an investigator can retrieve the
    /// exact log points behind the divergence. Absent when the divergence
    /// is not at a boundary or when either result predates moments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moments: Option<(String, String)>,
}

impl CampaignComparison {
    /// A compact, portable incident note suitable for an issue or CI artifact.
    pub fn markdown(&self) -> String {
        let mut report = format!(
            "# Theseus campaign comparison\n\nStatus: **{}**  \nLeft runs: {}  \nRight runs: {}\n",
            self.status, self.left_runs, self.right_runs
        );
        match &self.divergence {
            Some(divergence) => {
                report.push_str("\n## First recorded divergence\n\n");
                report.push_str(&format!(
                    "- Run: `{}`{}\n- Reason: {}\n- Left: `{}`\n- Right: `{}`\n",
                    divergence.run,
                    divergence
                        .boundary
                        .map(|boundary| format!("; boundary `{boundary}`"))
                        .unwrap_or_default(),
                    divergence.reason,
                    divergence.left,
                    divergence.right,
                ));
            }
            None => report.push_str("\nThe retained campaign evidence is identical.\n"),
        }
        report
    }

    /// GitHub Actions step-summary rendering: `::error` annotation for the
    /// divergence with both sides' evidence and moment addresses, so a
    /// cross-run comparison in CI flags the run directly.
    pub fn github(&self) -> String {
        let mut output = format!(
            "::warning title=Theseus comparison::status {} - left {} runs, right {} runs\n",
            self.status, self.left_runs, self.right_runs
        );
        if let Some(divergence) = &self.divergence {
            let boundary = divergence
                .boundary
                .map(|boundary| format!(" at boundary {boundary}"))
                .unwrap_or_default();
            output.push_str(&format!(
                "::error title=Theseus divergence{}::{}\n",
                boundary, divergence.reason
            ));
            output.push_str(&format!("left: {}\n", divergence.left));
            output.push_str(&format!("right: {}\n", divergence.right));
            if let Some((left_moment, right_moment)) = &divergence.moments {
                output.push_str(&format!(
                    "moments: left {left_moment} / right {right_moment}\n"
                ));
            }
        } else {
            output.push_str("The retained campaign evidence is identical.\n");
        }
        output
    }
}

/// Read the same RFC 6901 JSON Pointer from two locked campaign results.
/// The caller can inspect any retained timeline, action, property, coverage,
/// or serial-evidence field without extracting VM snapshots.
#[derive(Serialize)]
pub struct CampaignQuery {
    pub format: &'static str,
    pub pointer: String,
    pub equal: bool,
    pub left: Option<Value>,
    pub right: Option<Value>,
}

pub fn query_campaigns(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    pointer: &str,
) -> Result<CampaignQuery, CompareError> {
    let read = |root: &Path| -> Result<Value, CompareError> {
        Ok(serde_json::from_slice(&fs::read(
            root.join("campaign-result.json"),
        )?)?)
    };
    let left = read(left.as_ref())?.pointer(pointer).cloned();
    let right = read(right.as_ref())?.pointer(pointer).cloned();
    Ok(CampaignQuery {
        format: "theseus-campaign-query-v1",
        pointer: pointer.to_owned(),
        equal: left == right,
        left,
        right,
    })
}

pub fn compare_campaigns(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
) -> Result<CampaignComparison, CompareError> {
    let read = |root: &Path| -> Result<ResultFile, CompareError> {
        Ok(serde_json::from_slice(&fs::read(
            root.join("campaign-result.json"),
        )?)?)
    };
    let left = read(left.as_ref())?;
    let right = read(right.as_ref())?;
    let mut found_position: Option<usize> = None;
    let divergence = left
        .runs
        .iter()
        .zip(&right.runs)
        .enumerate()
        .find_map(|(position, (left, right))| {
            found_position = Some(position);
            let run = left.index.min(right.index).max(position);
            if left.operations != right.operations {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "selected operation history differs".to_owned(),
                    left: format!(
                        "operations={:?}; state={}",
                        left.operations, left.state_sha256
                    ),
                    right: format!(
                        "operations={:?}; state={}",
                        right.operations, right.state_sha256
                    ),
                    moments: None,
                });
            }
            if left.thread_schedule_prefixes != right.thread_schedule_prefixes {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "selected runnable thread prefix differs".to_owned(),
                    left: format!("prefixes={:?}", left.thread_schedule_prefixes),
                    right: format!("prefixes={:?}", right.thread_schedule_prefixes),
                    moments: None,
                });
            }
            if left.faults != right.faults {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "selected fault candidates differ".to_owned(),
                    left: format!("faults={:?}; state={}", left.faults, left.state_sha256),
                    right: format!("faults={:?}; state={}", right.faults, right.state_sha256),
                    moments: None,
                });
            }
            if left.actions != right.actions {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "selected fault actions differ".to_owned(),
                    left: json_summary(&left.actions),
                    right: json_summary(&right.actions),
                    moments: None,
                });
            }
            if left.execution_ledgers != right.execution_ledgers {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "ordered KVM execution ledger differs".to_owned(),
                    left: json_summary(&left.execution_ledgers),
                    right: json_summary(&right.execution_ledgers),
                    moments: None,
                });
            }
            if left.machine_execution_ledgers != right.machine_execution_ledgers {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "machine-wide execution stream differs".to_owned(),
                    left: json_summary(&left.machine_execution_ledgers),
                    right: json_summary(&right.machine_execution_ledgers),
                    moments: None,
                });
            }
            if left.machine_execution_traces != right.machine_execution_traces {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "actively enforced machine execution trace differs".to_owned(),
                    left: json_summary(&left.machine_execution_traces),
                    right: json_summary(&right.machine_execution_traces),
                    moments: None,
                });
            }
            for (boundary, (left, right)) in left.timeline.iter().zip(&right.timeline).enumerate() {
                if !left.id.is_empty() && !right.id.is_empty() && left.id != right.id {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "operation-boundary identities differ".to_owned(),
                        left: left.id.clone(),
                        right: right.id.clone(),
                        moments: None,
                    });
                }
                if left.actions != right.actions {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first operation-boundary fault actions differ".to_owned(),
                        left: json_summary(&left.actions),
                        right: json_summary(&right.actions),
                        moments: None,
                    });
                }
                if left.execution_ledgers != right.execution_ledgers {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "ordered KVM execution ledger differs".to_owned(),
                        left: json_summary(&left.execution_ledgers),
                        right: json_summary(&right.execution_ledgers),
                        moments: None,
                    });
                }
                if left.machine_execution_ledgers != right.machine_execution_ledgers {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "machine-wide execution stream differs".to_owned(),
                        left: json_summary(&left.machine_execution_ledgers),
                        right: json_summary(&right.machine_execution_ledgers),
                        moments: None,
                    });
                }
                if left.operation != right.operation
                    || left.service != right.service
                    || left.state_sha256 != right.state_sha256
                {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first operation-boundary state differs".to_owned(),
                        left: format!(
                            "{}@{} state={}",
                            left.operation, left.service, left.state_sha256
                        ),
                        right: format!(
                            "{}@{} state={}",
                            right.operation, right.service, right.state_sha256
                        ),
                        moments: None,
                    });
                }
                if left.serial_sha256 != right.serial_sha256 || left.markers != right.markers {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first operation-boundary evidence differs".to_owned(),
                        left: format!(
                            "markers={:?}; serial={:?}",
                            left.markers, left.serial_sha256
                        ),
                        right: format!(
                            "markers={:?}; serial={:?}",
                            right.markers, right.serial_sha256
                        ),
                        moments: None,
                    });
                }
                if left.program_counters != right.program_counters
                    || left.instruction_locations != right.instruction_locations
                    || left.application_blocks != right.application_blocks
                {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first operation-boundary coverage differs".to_owned(),
                        left: format!(
                            "program_counters={}; instruction_locations={}; application_blocks={}",
                            json_summary(&left.program_counters),
                            json_summary(&left.instruction_locations),
                            json_summary(&left.application_blocks)
                        ),
                        right: format!(
                            "program_counters={}; instruction_locations={}; application_blocks={}",
                            json_summary(&right.program_counters),
                            json_summary(&right.instruction_locations),
                            json_summary(&right.application_blocks)
                        ),
                        moments: None,
                    });
                }
                if left.structured_choices != right.structured_choices {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first structured choice differs".to_owned(),
                        left: json_summary(&left.structured_choices),
                        right: json_summary(&right.structured_choices),
                        moments: None,
                    });
                }
                if left.thread_scheduling != right.thread_scheduling {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first thread-scheduling decision differs".to_owned(),
                        left: json_summary(&left.thread_scheduling),
                        right: json_summary(&right.thread_scheduling),
                        moments: None,
                    });
                }
                if left.thread_synchronization != right.thread_synchronization {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first thread-synchronization event differs".to_owned(),
                        left: json_summary(&left.thread_synchronization),
                        right: json_summary(&right.thread_synchronization),
                        moments: None,
                    });
                }
            }
            if left.timeline.len() != right.timeline.len()
                || left.state_sha256 != right.state_sha256
            {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "final topology state differs".to_owned(),
                    left: left.state_sha256.clone(),
                    right: right.state_sha256.clone(),
                    moments: None,
                });
            }
            if left.property_witnesses != right.property_witnesses {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "property witnesses differ".to_owned(),
                    left: format!("{:?}", left.property_witnesses),
                    right: format!("{:?}", right.property_witnesses),
                    moments: None,
                });
            }
            if left.structured_choices != right.structured_choices {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "structured choices differ".to_owned(),
                    left: json_summary(&left.structured_choices),
                    right: json_summary(&right.structured_choices),
                    moments: None,
                });
            }
            if left.thread_scheduling != right.thread_scheduling {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "thread-scheduling decisions differ".to_owned(),
                    left: json_summary(&left.thread_scheduling),
                    right: json_summary(&right.thread_scheduling),
                    moments: None,
                });
            }
            if left.thread_synchronization != right.thread_synchronization {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "thread-synchronization events differ".to_owned(),
                    left: json_summary(&left.thread_synchronization),
                    right: json_summary(&right.thread_synchronization),
                    moments: None,
                });
            }
            if left.program_counters != right.program_counters
                || left.instruction_locations != right.instruction_locations
                || left.instruction_novelty != right.instruction_novelty
                || left.checkpoint_pc_novelty != right.checkpoint_pc_novelty
                || left.application_blocks != right.application_blocks
                || left.application_block_novelty != right.application_block_novelty
            {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "accumulated campaign coverage differs".to_owned(),
                    left: coverage_summary(left),
                    right: coverage_summary(right),
                    moments: None,
                });
            }
            if (!left.decision_trace.is_empty() || !right.decision_trace.is_empty())
                && left.decision_trace != right.decision_trace
            {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "decision trace differs".to_owned(),
                    left: format!("{:?}", left.decision_trace),
                    right: format!("{:?}", right.decision_trace),
                    moments: None,
                });
            }
            None
        })
        .or_else(|| {
            (left.properties != right.properties).then(|| CampaignDivergence {
                run: 0,
                boundary: None,
                reason: "campaign property verdicts differ".to_owned(),
                left: json_summary(&left.properties),
                right: json_summary(&right.properties),
                moments: None,
            })
        })
        .or_else(|| {
            (left.runs.len() != right.runs.len()).then(|| CampaignDivergence {
                run: left.runs.len().min(right.runs.len()),
                boundary: None,
                reason: "campaign run count differs".to_owned(),
                left: left.runs.len().to_string(),
                right: right.runs.len().to_string(),
                moments: None,
            })
        });
    let divergence = divergence.map(|mut divergence| {
        divergence.moments = found_position.and_then(|position| {
            let boundary = divergence.boundary?;
            let moment = |runs: &[Run], position: usize, boundary: usize| -> String {
                runs.get(position)
                    .and_then(|run| run.timeline.get(boundary))
                    .map(|boundary| boundary.moment.clone())
                    .unwrap_or_default()
            };
            let left_moment = moment(&left.runs, position, boundary);
            let right_moment = moment(&right.runs, position, boundary);
            if left_moment.is_empty() && right_moment.is_empty() {
                None
            } else {
                Some((left_moment, right_moment))
            }
        });
        divergence
    });
    Ok(CampaignComparison {
        format: "theseus-campaign-comparison-v1",
        status: if divergence.is_some() {
            "diverged"
        } else {
            "same"
        },
        left_runs: left.runs.len(),
        right_runs: right.runs.len(),
        divergence,
    })
}

fn json_summary(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unencodable evidence>".to_owned())
}

/// The diff of one counterfactual future against the retained base run it
/// forked. The two futures are expected to differ at the substituted
/// decision; this comparison locates the first diverging operation boundary
/// and reports both sides' moment addresses, so the same address retrieves
/// either future's retained evidence.
#[derive(Debug, Serialize)]
pub struct ForkedComparison {
    pub format: &'static str,
    pub status: &'static str,
    /// Run index in the base campaign whose recorded future was re-executed.
    pub forked_run: usize,
    pub replaced_fault: String,
    pub replacement_fault: String,
    pub left_runs: usize,
    pub right_runs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub divergence: Option<CampaignDivergence>,
}

struct ForkedDivergence {
    boundary: Option<usize>,
    reason: String,
    base: String,
    fork: String,
    /// `(base, fork)` moment addresses at the diverging boundary.
    moments: Option<(String, String)>,
}

fn forked_boundary_divergence(
    boundary: usize,
    reason: &str,
    base: String,
    fork: String,
    base_boundary: &Boundary,
    fork_boundary: &Boundary,
) -> ForkedDivergence {
    let moments = if base_boundary.moment.is_empty() && fork_boundary.moment.is_empty() {
        None
    } else {
        Some((base_boundary.moment.clone(), fork_boundary.moment.clone()))
    };
    ForkedDivergence {
        boundary: Some(boundary),
        reason: reason.to_owned(),
        base,
        fork,
        moments,
    }
}

/// Walk the forked future against its base run and report the first
/// operation-boundary divergence. The substituted fault decision itself is
/// expected to differ, so run-level decision records (faults, actions,
/// decision trace, machine traces, accumulated coverage) are not divergences:
/// the retained boundaries are where the futures become distinguishable.
fn forked_divergence(base: &Run, fork: &Run) -> Option<ForkedDivergence> {
    if base.operations != fork.operations {
        return Some(ForkedDivergence {
            boundary: None,
            reason: "the fork left its recorded operation history".to_owned(),
            base: format!("operations={:?}", base.operations),
            fork: format!("operations={:?}", fork.operations),
            moments: None,
        });
    }
    if base.thread_schedule_prefixes != fork.thread_schedule_prefixes {
        return Some(ForkedDivergence {
            boundary: None,
            reason: "the fork left its recorded runnable thread prefixes".to_owned(),
            base: format!("prefixes={:?}", base.thread_schedule_prefixes),
            fork: format!("prefixes={:?}", fork.thread_schedule_prefixes),
            moments: None,
        });
    }
    for (boundary, (base, fork)) in base.timeline.iter().zip(&fork.timeline).enumerate() {
        if !base.id.is_empty() && !fork.id.is_empty() && base.id != fork.id {
            return Some(forked_boundary_divergence(
                boundary,
                "operation-boundary identities diverge",
                base.id.clone(),
                fork.id.clone(),
                base,
                fork,
            ));
        }
        if base.actions != fork.actions {
            return Some(forked_boundary_divergence(
                boundary,
                "operation-boundary fault actions diverge",
                json_summary(&base.actions),
                json_summary(&fork.actions),
                base,
                fork,
            ));
        }
        if base.execution_ledgers != fork.execution_ledgers {
            return Some(forked_boundary_divergence(
                boundary,
                "ordered KVM execution ledger diverges",
                json_summary(&base.execution_ledgers),
                json_summary(&fork.execution_ledgers),
                base,
                fork,
            ));
        }
        if base.machine_execution_ledgers != fork.machine_execution_ledgers {
            return Some(forked_boundary_divergence(
                boundary,
                "machine-wide execution stream diverges",
                json_summary(&base.machine_execution_ledgers),
                json_summary(&fork.machine_execution_ledgers),
                base,
                fork,
            ));
        }
        if base.operation != fork.operation
            || base.service != fork.service
            || base.state_sha256 != fork.state_sha256
        {
            return Some(forked_boundary_divergence(
                boundary,
                "operation-boundary state diverges",
                format!(
                    "{}@{} state={}",
                    base.operation, base.service, base.state_sha256
                ),
                format!(
                    "{}@{} state={}",
                    fork.operation, fork.service, fork.state_sha256
                ),
                base,
                fork,
            ));
        }
        if base.serial_sha256 != fork.serial_sha256 || base.markers != fork.markers {
            return Some(forked_boundary_divergence(
                boundary,
                "operation-boundary evidence diverges",
                format!(
                    "markers={:?}; serial={:?}",
                    base.markers, base.serial_sha256
                ),
                format!(
                    "markers={:?}; serial={:?}",
                    fork.markers, fork.serial_sha256
                ),
                base,
                fork,
            ));
        }
        if base.program_counters != fork.program_counters
            || base.instruction_locations != fork.instruction_locations
            || base.application_blocks != fork.application_blocks
        {
            return Some(forked_boundary_divergence(
                boundary,
                "operation-boundary coverage diverges",
                format!(
                    "program_counters={}; instruction_locations={}; application_blocks={}",
                    json_summary(&base.program_counters),
                    json_summary(&base.instruction_locations),
                    json_summary(&base.application_blocks)
                ),
                format!(
                    "program_counters={}; instruction_locations={}; application_blocks={}",
                    json_summary(&fork.program_counters),
                    json_summary(&fork.instruction_locations),
                    json_summary(&fork.application_blocks)
                ),
                base,
                fork,
            ));
        }
        if base.structured_choices != fork.structured_choices {
            return Some(forked_boundary_divergence(
                boundary,
                "structured choices diverge",
                json_summary(&base.structured_choices),
                json_summary(&fork.structured_choices),
                base,
                fork,
            ));
        }
        if base.thread_scheduling != fork.thread_scheduling {
            return Some(forked_boundary_divergence(
                boundary,
                "thread-scheduling decisions diverge",
                json_summary(&base.thread_scheduling),
                json_summary(&fork.thread_scheduling),
                base,
                fork,
            ));
        }
        if base.thread_synchronization != fork.thread_synchronization {
            return Some(forked_boundary_divergence(
                boundary,
                "thread-synchronization events diverge",
                json_summary(&base.thread_synchronization),
                json_summary(&fork.thread_synchronization),
                base,
                fork,
            ));
        }
    }
    if base.timeline.len() != fork.timeline.len() {
        return Some(ForkedDivergence {
            boundary: None,
            reason: "operation-boundary timelines diverge in length".to_owned(),
            base: base.timeline.len().to_string(),
            fork: fork.timeline.len().to_string(),
            moments: None,
        });
    }
    None
}

/// Diff a counterfactual future against the retained base campaign it forked.
/// Exactly one side must carry the fork's counterfactual provenance; the
/// comparison locates the base run by that provenance rather than by
/// position, reports the first diverging operation boundary in argument
/// order, and attaches both sides' moment addresses.
pub fn compare_forked_campaigns(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
) -> Result<ForkedComparison, CompareError> {
    let read = |root: &Path| -> Result<ResultFile, CompareError> {
        Ok(serde_json::from_slice(&fs::read(
            root.join("campaign-result.json"),
        )?)?)
    };
    let left = read(left.as_ref())?;
    let right = read(right.as_ref())?;
    let (base, fork, provenance, fork_is_right) = match (
        left.counterfactual.clone(),
        right.counterfactual.clone(),
    ) {
        (None, Some(provenance)) => (&left, &right, provenance, true),
        (Some(provenance), None) => (&right, &left, provenance, false),
        (Some(_), Some(_)) => {
            return Err(CompareError::Invalid(
                "both results record counterfactual provenance; compare the forked future against its unmodified base campaign".to_owned(),
            ));
        }
        (None, None) => {
            return Err(CompareError::Invalid(
                "neither result records counterfactual provenance; fork a future with `theseus compose explore --fork-run N --replace-fault OLD=NEW` first".to_owned(),
            ));
        }
    };
    let base_run = base.runs.get(provenance.run).ok_or_else(|| {
        CompareError::Invalid(format!(
            "forked run index {} is outside the base campaign ({} runs)",
            provenance.run,
            base.runs.len()
        ))
    })?;
    let fork_run = fork
        .runs
        .first()
        .ok_or_else(|| CompareError::Invalid("forked campaign has no runs".to_owned()))?;
    let divergence = forked_divergence(base_run, fork_run).map(|divergence| {
        let (left, right, moments) = if fork_is_right {
            (divergence.base, divergence.fork, divergence.moments)
        } else {
            (
                divergence.fork,
                divergence.base,
                divergence
                    .moments
                    .map(|(base_moment, fork_moment)| (fork_moment, base_moment)),
            )
        };
        CampaignDivergence {
            run: provenance.run,
            boundary: divergence.boundary,
            reason: divergence.reason,
            left,
            right,
            moments,
        }
    });
    Ok(ForkedComparison {
        format: "theseus-campaign-forked-comparison-v1",
        status: if divergence.is_some() {
            "diverged"
        } else {
            "same"
        },
        forked_run: provenance.run,
        replaced_fault: provenance.fault,
        replacement_fault: provenance.replace,
        left_runs: left.runs.len(),
        right_runs: right.runs.len(),
        divergence,
    })
}

fn coverage_summary(run: &Run) -> String {
    format!(
        "program_counters={}; instruction_locations={}; instruction_novelty={:?}; checkpoint_pc_novelty={:?}; application_blocks={}; application_block_novelty={:?}",
        json_summary(&run.program_counters),
        json_summary(&run.instruction_locations),
        run.instruction_novelty,
        run.checkpoint_pc_novelty,
        json_summary(&run.application_blocks),
        run.application_block_novelty,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(runs: &str, properties: &str) -> String {
        format!(r#"{{"runs":{runs},"properties":{properties}}}"#)
    }

    fn write_pair(
        left_contents: &str,
        right_contents: &str,
    ) -> (tempfile::TempDir, tempfile::TempDir) {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        fs::write(left.path().join("campaign-result.json"), left_contents).unwrap();
        fs::write(right.path().join("campaign-result.json"), right_contents).unwrap();
        (left, right)
    }

    #[test]
    fn github_format_annotations_report_divergence_and_moments() {
        let baseline = r#"[{"index":0,"operations":["write"],"state_sha256":"final","timeline":[{"id":"op-000-write","operation":"write","service":"api","state_sha256":"state","moment":"7000@input-hash","actions":[{"kind":"partition"}],"markers":["42"],"serial_sha256":{"api":"serial"},"program_counters":{"api":["0x10"]}}]}]"#;
        let right = baseline
            .replace(r#""state_sha256":"state""#, r#""state_sha256":"changed""#)
            .replace("7000@input-hash", "9000@input-hash");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&right, "[]"));
        let comparison = compare_campaigns(left.path(), right.path()).unwrap();
        let output = comparison.github();
        assert!(
            output.contains("::warning title=Theseus comparison::"),
            "{output}"
        );
        assert!(
            output.contains("::error title=Theseus divergence"),
            "{output}"
        );
        assert!(
            output.contains("moments: left 7000@input-hash / right 9000@input-hash"),
            "{output}"
        );
        assert!(output.contains("left: "), "{output}");
        assert!(output.contains("right: "), "{output}");
    }

    #[test]
    fn at_moment_dumps_both_boundary_records() {
        let baseline = r#"[{"index":0,"operations":["write"],"state_sha256":"final","timeline":[{"id":"op-000-write","operation":"write","service":"api","state_sha256":"state","moment":"7000@input-hash","actions":[{"kind":"partition"}],"markers":["42"],"serial_sha256":{"api":"serial"},"program_counters":{"api":["0x10"]}}]}]"#;
        let right = baseline
            .replace("\"state_sha256\":\"state\"", "\"state_sha256\":\"changed\"")
            .replace("7000@input-hash", "7000@input-hash");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&right, "[]"));
        let diff = boundary_at_moment(left.path(), right.path(), "7000@input-hash").unwrap();
        assert_eq!(diff.run, 0);
        assert_eq!(diff.boundary, "op-000-write");
        assert!(!diff.identical);
        assert_eq!(diff.left["state_sha256"], "state");
        assert_eq!(diff.right["state_sha256"], "changed");

        // An address only one side carries is an error naming the mismatch.
        let drifted = baseline.replace("7000@input-hash", "9000@other-hash");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&drifted, "[]"));
        let error = boundary_at_moment(left.path(), right.path(), "7000@input-hash").unwrap_err();
        assert!(error.to_string().contains("no boundary carries"), "{error}");
    }

    #[test]
    fn boundary_divergences_report_both_moment_addresses() {
        let baseline = r#"[{"index":0,"operations":["write"],"faults":["partition"],"state_sha256":"final","timeline":[{"operation":"write","service":"api","state_sha256":"state","moment":"7000@input-hash","actions":[{"kind":"partition"}],"markers":["42"],"serial_sha256":{"api":"serial"},"program_counters":{"api":["0x10"]}}]}]"#;
        // Same shape but a different state at the same boundary, with the
        // right side carrying its own moment.
        let right = baseline
            .replace(r#""state_sha256":"state""#, r#""state_sha256":"changed""#)
            .replace("7000@input-hash", "9000@input-hash");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&right, "[]"));
        let divergence = compare_campaigns(left.path(), right.path())
            .unwrap()
            .divergence
            .unwrap();
        assert_eq!(
            divergence.moments,
            Some(("7000@input-hash".to_owned(), "9000@input-hash".to_owned()))
        );
    }

    #[test]
    fn reports_the_first_different_campaign_run() {
        let (left, right) = write_pair(
            &result(
                r#"[{"index":0,"operations":["write"],"state_sha256":"same"},{"index":1,"operations":["read"],"state_sha256":"left"}]"#,
                "[]",
            ),
            &result(
                r#"[{"index":0,"operations":["write"],"state_sha256":"same"},{"index":1,"operations":["retry"],"state_sha256":"right"}]"#,
                "[]",
            ),
        );
        let comparison = compare_campaigns(left.path(), right.path()).unwrap();
        assert_eq!(comparison.status, "diverged");
        assert_eq!(comparison.divergence.unwrap().run, 1);
    }

    #[test]
    fn reports_the_first_boundary_fault_coverage_and_serial_difference() {
        let baseline = r#"[{"index":0,"operations":["write"],"faults":["partition"],"state_sha256":"final","timeline":[{"operation":"write","service":"api","state_sha256":"state","actions":[{"kind":"partition"}],"markers":["42"],"serial_sha256":{"api":"serial"},"program_counters":{"api":["0x10"]}}]}]"#;
        let changed_action = baseline.replace("partition\"}]", "heal\"}]");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&changed_action, "[]"));
        assert_eq!(
            compare_campaigns(left.path(), right.path())
                .unwrap()
                .divergence
                .unwrap()
                .reason,
            "first operation-boundary fault actions differ"
        );

        let changed_state = baseline.replace("state\"", "other-state\"");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&changed_state, "[]"));
        assert_eq!(
            compare_campaigns(left.path(), right.path())
                .unwrap()
                .divergence
                .unwrap()
                .reason,
            "first operation-boundary state differs"
        );

        let changed_coverage = baseline.replace("0x10", "0x20");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&changed_coverage, "[]"));
        assert_eq!(
            compare_campaigns(left.path(), right.path())
                .unwrap()
                .divergence
                .unwrap()
                .reason,
            "first operation-boundary coverage differs"
        );

        let changed_serial = baseline.replace("serial\"}", "other\"}");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&changed_serial, "[]"));
        assert_eq!(
            compare_campaigns(left.path(), right.path())
                .unwrap()
                .divergence
                .unwrap()
                .reason,
            "first operation-boundary evidence differs"
        );
    }

    #[test]
    fn reports_selected_fault_candidates_before_execution_evidence() {
        let runs =
            r#"[{"index":0,"operations":["write"],"faults":["partition"],"state_sha256":"same"}]"#;
        let changed = runs.replace("partition", "heal");
        let (left, right) = write_pair(&result(runs, "[]"), &result(&changed, "[]"));
        assert_eq!(
            compare_campaigns(left.path(), right.path())
                .unwrap()
                .divergence
                .unwrap()
                .reason,
            "selected fault candidates differ"
        );
    }

    #[test]
    fn reports_the_first_changed_thread_schedule_choice() {
        let digest = "0123456789abcdef".repeat(4);
        let runs = format!(
            r#"[{{"index":0,"operations":["deposit"],"state_sha256":"same","thread_scheduling":{{"ledger":[{{"process":"ledger","module":"deposit","build_sha256":"{digest}","decision":4,"from_thread":1,"runnable_mask":"0x00000006","selected_thread":2,"point_offset":"0x42"}}]}},"timeline":[{{"operation":"deposit","service":"ledger","state_sha256":"same","thread_scheduling":{{"ledger":[{{"process":"ledger","module":"deposit","build_sha256":"{digest}","decision":4,"from_thread":1,"runnable_mask":"0x00000006","selected_thread":2,"point_offset":"0x42"}}]}}}}]}}]"#
        );
        let changed = runs.replace("\"selected_thread\":2", "\"selected_thread\":1");
        let (left, right) = write_pair(&result(&runs, "[]"), &result(&changed, "[]"));
        let divergence = compare_campaigns(left.path(), right.path())
            .unwrap()
            .divergence
            .unwrap();
        assert_eq!(divergence.boundary, Some(0));
        assert_eq!(
            divergence.reason,
            "first thread-scheduling decision differs"
        );
    }

    #[test]
    fn reports_the_first_changed_structured_choice() {
        let runs = r#"[{"index":0,"operations":["calculate"],"state_sha256":"same","structured_choices":{"chooser":[{"ordinal":0,"name":"mode","upper_exclusive":2,"selected":1}]},"timeline":[{"operation":"calculate","service":"chooser","state_sha256":"same","structured_choices":{"chooser":[{"ordinal":0,"name":"mode","upper_exclusive":2,"selected":1}]}}]}]"#;
        let changed = runs.replace("\"selected\":1", "\"selected\":0");
        let (left, right) = write_pair(&result(runs, "[]"), &result(&changed, "[]"));
        let divergence = compare_campaigns(left.path(), right.path())
            .unwrap()
            .divergence
            .unwrap();
        assert_eq!(divergence.boundary, Some(0));
        assert_eq!(divergence.reason, "first structured choice differs");
    }

    #[test]
    fn reports_the_first_changed_ordered_kvm_execution_ledger() {
        let digest = "0123456789abcdef".repeat(4);
        let runs = format!(
            r#"[{{"index":0,"operations":["write"],"state_sha256":"same","timeline":[{{"operation":"write","service":"api","state_sha256":"same","execution_ledgers":{{"api":[{{"decisions":12,"sha256":"{digest}","tail":["mmio_read:0x10:1"]}}]}}}}]}}]"#
        );
        let changed = runs.replace("\"decisions\":12", "\"decisions\":13");
        let (left, right) = write_pair(&result(&runs, "[]"), &result(&changed, "[]"));
        let divergence = compare_campaigns(left.path(), right.path())
            .unwrap()
            .divergence
            .unwrap();
        assert_eq!(divergence.boundary, Some(0));
        assert_eq!(divergence.reason, "ordered KVM execution ledger differs");
    }

    #[test]
    fn reports_the_first_changed_machine_wide_execution_stream() {
        let digest = "0123456789abcdef".repeat(4);
        let runs = format!(
            r#"[{{"index":0,"operations":["write"],"state_sha256":"same","timeline":[{{"operation":"write","service":"api","state_sha256":"same","machine_execution_ledgers":{{"api":{{"decisions":12,"sha256":"{digest}","tail":["vcpu:0:mmio_read:0x10:1"]}}}}}}]}}]"#
        );
        let changed = runs.replace("\"vcpu:0:", "\"vcpu:1:");
        let (left, right) = write_pair(&result(&runs, "[]"), &result(&changed, "[]"));
        let divergence = compare_campaigns(left.path(), right.path())
            .unwrap()
            .divergence
            .unwrap();
        assert_eq!(divergence.boundary, Some(0));
        assert_eq!(divergence.reason, "machine-wide execution stream differs");
    }

    #[test]
    fn reports_the_first_changed_active_machine_replay_trace() {
        let runs = r#"[{"index":0,"operations":["write"],"state_sha256":"same","machine_execution_traces":{"api":["vcpu:0:mmio_write:0x10:1:2a"]}}]"#;
        let changed = runs.replace("vcpu:0:", "vcpu:1:");
        let (left, right) = write_pair(&result(runs, "[]"), &result(&changed, "[]"));
        let divergence = compare_campaigns(left.path(), right.path())
            .unwrap()
            .divergence
            .unwrap();
        assert_eq!(divergence.boundary, None);
        assert_eq!(
            divergence.reason,
            "actively enforced machine execution trace differs"
        );
    }

    #[test]
    fn reports_the_first_changed_thread_synchronization_event() {
        let digest = "0123456789abcdef".repeat(4);
        let runs = format!(
            r#"[{{"index":0,"operations":["workers"],"state_sha256":"same","thread_synchronization":{{"workers":[{{"process":"workers","module":"condition","build_sha256":"{digest}","event":4,"thread":2,"operation":"signal","object_kind":"condition","object":1,"peer_thread":1}}]}},"timeline":[{{"operation":"workers","service":"workers","state_sha256":"same","thread_synchronization":{{"workers":[{{"process":"workers","module":"condition","build_sha256":"{digest}","event":4,"thread":2,"operation":"signal","object_kind":"condition","object":1,"peer_thread":1}}]}}}}]}}]"#
        );
        let changed = runs.replace("\"peer_thread\":1", "\"peer_thread\":0");
        let (left, right) = write_pair(&result(&runs, "[]"), &result(&changed, "[]"));
        let divergence = compare_campaigns(left.path(), right.path())
            .unwrap()
            .divergence
            .unwrap();
        assert_eq!(divergence.boundary, Some(0));
        assert_eq!(
            divergence.reason,
            "first thread-synchronization event differs"
        );
    }

    #[test]
    fn reports_a_changed_runnable_prefix_before_execution_evidence() {
        let baseline = r#"[{"index":0,"operations":["deposit"],"thread_schedule_prefixes":[[0,1]],"state_sha256":"same"}]"#;
        let changed = baseline.replace("[0,1]", "[0,2]");
        let (left, right) = write_pair(&result(baseline, "[]"), &result(&changed, "[]"));
        assert_eq!(
            compare_campaigns(left.path(), right.path())
                .unwrap()
                .divergence
                .unwrap()
                .reason,
            "selected runnable thread prefix differs"
        );
    }

    #[test]
    fn reports_property_verdicts_and_queries_retained_evidence() {
        let runs = r#"[{"index":0,"operations":["read"],"state_sha256":"same","property_witnesses":["stale"]}]"#;
        let (left, right) = write_pair(
            &result(
                runs,
                r#"[{"name":"consistent_read","kind":"always","status":"passed","detail":"ok"}]"#,
            ),
            &result(
                runs,
                r#"[{"name":"consistent_read","kind":"always","status":"failed","detail":"stale"}]"#,
            ),
        );
        let comparison = compare_campaigns(left.path(), right.path()).unwrap();
        assert_eq!(
            comparison.divergence.as_ref().unwrap().reason,
            "campaign property verdicts differ"
        );
        assert!(comparison.markdown().contains("First recorded divergence"));

        let query = query_campaigns(left.path(), right.path(), "/properties/0/status").unwrap();
        assert!(!query.equal);
        assert_eq!(query.left, Some(Value::String("passed".to_owned())));
        assert_eq!(query.right, Some(Value::String("failed".to_owned())));
    }

    fn forked_result(
        runs: &str,
        run: usize,
        fault: &str,
        replace: &str,
    ) -> String {
        format!(
            r#"{{"counterfactual":{{"run":{run},"fault":"{fault}","replace":"{replace}"}},"runs":{runs},"properties":[]}}"#
        )
    }

    const FORKED_BASE_RUN: &str = r#"[{"index":0,"operations":["write"],"faults":["backplane:partition@write"],"state_sha256":"base-final","timeline":[
            {"id":"op-000-write","operation":"write","service":"api","state_sha256":"state","moment":"7000@input-hash","actions":[{"kind":"partition"}],"markers":["42"],"serial_sha256":{"api":"serial"},"program_counters":{"api":["0x10"]}},
            {"id":"op-001-read","operation":"read","service":"api","state_sha256":"state","moment":"7200@second-hash","actions":[],"markers":[],"serial_sha256":{"api":"serial"}}
        ]}]"#;

    #[test]
    fn forked_comparison_reports_the_substituted_decision_boundary_with_both_moments() {
        // The fork reuses the recorded operation history and prefix, but the
        // replacement fault takes the replaced fault's place at the first
        // barrier and its future moves to its own moment.
        let fork_runs = FORKED_BASE_RUN
            .replace(r#""kind":"partition""#, r#""kind":"heal""#)
            .replace("7000@input-hash", "7100@fork-hash");
        let (base, forked) = write_pair(
            &result(FORKED_BASE_RUN, "[]"),
            &forked_result(
                &fork_runs,
                0,
                "backplane:partition@write",
                "backplane:heal@write",
            ),
        );
        let comparison = compare_forked_campaigns(base.path(), forked.path()).unwrap();
        assert_eq!(comparison.status, "diverged");
        assert_eq!(comparison.forked_run, 0);
        assert_eq!(
            comparison.replaced_fault,
            "backplane:partition@write"
        );
        assert_eq!(comparison.replacement_fault, "backplane:heal@write");
        let divergence = comparison.divergence.unwrap();
        assert_eq!(divergence.boundary, Some(0));
        assert_eq!(
            divergence.reason,
            "operation-boundary fault actions diverge"
        );
        assert_eq!(
            divergence.moments,
            Some(("7000@input-hash".to_owned(), "7100@fork-hash".to_owned()))
        );
    }

    #[test]
    fn forked_comparison_reports_identical_boundary_evidence_as_same() {
        // A substitution whose effects are invisible at every retained
        // boundary - for example a lifecycle pause - keeps both futures
        // indistinguishable in the retained evidence.
        let (base, forked) = write_pair(
            &result(FORKED_BASE_RUN, "[]"),
            &forked_result(
                FORKED_BASE_RUN,
                0,
                "guest:pause@write",
                "guest:restart@write",
            ),
        );
        let comparison = compare_forked_campaigns(base.path(), forked.path()).unwrap();
        assert_eq!(comparison.status, "same");
        assert!(comparison.divergence.is_none());
    }

    #[test]
    fn forked_comparison_accepts_the_fork_on_either_side_and_reports_in_argument_order() {
        let fork_runs = FORKED_BASE_RUN
            .replace(r#""kind":"partition""#, r#""kind":"heal""#)
            .replace("7000@input-hash", "7100@fork-hash");
        // Fork on the left: its evidence is reported first.
        let (forked, base) = write_pair(
            &forked_result(&fork_runs, 0, "guest:pause@write", "guest:restart@write"),
            &result(FORKED_BASE_RUN, "[]"),
        );
        let comparison = compare_forked_campaigns(forked.path(), base.path()).unwrap();
        let divergence = comparison.divergence.expect("the futures diverge at the barrier");
        assert_eq!(divergence.boundary, Some(0));
        assert_eq!(divergence.left, r#"[{"kind":"heal"}]"#);
        assert_eq!(divergence.right, r#"[{"kind":"partition"}]"#);
        assert_eq!(
            divergence.moments,
            Some(("7100@fork-hash".to_owned(), "7000@input-hash".to_owned()))
        );
    }

    #[test]
    fn forked_comparison_requires_counterfactual_provenance() {
        let (left, right) = write_pair(&result(FORKED_BASE_RUN, "[]"), &result(FORKED_BASE_RUN, "[]"));
        let error =
            compare_forked_campaigns(left.path(), right.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("neither result records counterfactual provenance"),
            "{error}"
        );

        // A fork index outside the base campaign names the mismatch.
        let forked = write_pair(
            &result(FORKED_BASE_RUN, "[]"),
            &forked_result(FORKED_BASE_RUN, 3, "a", "b"),
        );
        let error = compare_forked_campaigns(forked.0.path(), forked.1.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("forked run index 3 is outside the base campaign"),
            "{error}"
        );
    }
}

/// Dump both sides' full boundary record at one moment address, so an
/// investigator can diff everything that differs at that exact point -
/// not just the first divergence the comparison stops at.
pub fn boundary_at_moment(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    moment: &str,
) -> Result<BoundaryMomentDiff, CompareError> {
    let read = |root: &Path| -> Result<serde_json::Value, CompareError> {
        Ok(serde_json::from_slice(&fs::read(
            root.join("campaign-result.json"),
        )?)?)
    };
    let left_result = read(left.as_ref())?;
    let right_result = read(right.as_ref())?;
    let left_boundary = find_boundary_moment(&left_result, moment)?;
    let right_boundary = find_boundary_moment(&right_result, moment)?;
    if left_boundary.run != right_boundary.run {
        return Err(CompareError::Invalid(format!(
            "moment {moment:?} resolves to run {} on the left and run {} on the right",
            left_boundary.run, right_boundary.run
        )));
    }
    Ok(BoundaryMomentDiff {
        run: left_boundary.run,
        boundary: left_boundary.boundary_id.clone(),
        moment: moment.to_owned(),
        identical: left_boundary.record == right_boundary.record,
        left: left_boundary.record,
        right: right_boundary.record,
    })
}

struct BoundaryRecord {
    run: usize,
    boundary_id: String,
    record: serde_json::Value,
}

fn find_boundary_moment(
    result: &serde_json::Value,
    moment: &str,
) -> Result<BoundaryRecord, CompareError> {
    let runs = result["runs"]
        .as_array()
        .ok_or_else(|| CompareError::Invalid("result has no runs".to_owned()))?;
    for (run_index, run) in runs.iter().enumerate() {
        let timeline = run["timeline"]
            .as_array()
            .ok_or_else(|| CompareError::Invalid(format!("run {run_index} has no timeline")))?;
        for boundary in timeline {
            if boundary["moment"] == *moment {
                return Ok(BoundaryRecord {
                    run: run_index,
                    boundary_id: boundary["id"].as_str().unwrap_or_default().to_owned(),
                    record: boundary.clone(),
                });
            }
        }
    }
    Err(CompareError::Invalid(format!(
        "no boundary carries moment {moment:?}"
    )))
}

#[derive(Debug, Serialize, PartialEq)]
pub struct BoundaryMomentDiff {
    pub run: usize,
    pub boundary: String,
    pub moment: String,
    pub identical: bool,
    pub left: serde_json::Value,
    pub right: serde_json::Value,
}
