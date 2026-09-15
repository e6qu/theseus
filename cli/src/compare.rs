use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

type Coverage = std::collections::BTreeMap<String, Vec<Value>>;

#[derive(Debug)]
pub enum CompareError {
    Read(std::io::Error),
    Parse(serde_json::Error),
}

impl std::fmt::Display for CompareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "cannot read campaign result: {error}"),
            Self::Parse(error) => write!(formatter, "cannot parse campaign result: {error}"),
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
    state_sha256: String,
    #[serde(default)]
    timeline: Vec<Boundary>,
}
#[derive(Deserialize)]
struct Boundary {
    #[serde(default)]
    id: String,
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
#[derive(Serialize)]
pub struct CampaignDivergence {
    pub run: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<usize>,
    pub reason: String,
    pub left: String,
    pub right: String,
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
    let divergence = left
        .runs
        .iter()
        .zip(&right.runs)
        .enumerate()
        .find_map(|(position, (left, right))| {
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
                });
            }
            if left.thread_schedule_prefixes != right.thread_schedule_prefixes {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "selected runnable thread prefix differs".to_owned(),
                    left: format!("prefixes={:?}", left.thread_schedule_prefixes),
                    right: format!("prefixes={:?}", right.thread_schedule_prefixes),
                });
            }
            if left.faults != right.faults {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "selected fault candidates differ".to_owned(),
                    left: format!("faults={:?}; state={}", left.faults, left.state_sha256),
                    right: format!("faults={:?}; state={}", right.faults, right.state_sha256),
                });
            }
            if left.actions != right.actions {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "selected fault actions differ".to_owned(),
                    left: json_summary(&left.actions),
                    right: json_summary(&right.actions),
                });
            }
            if left.execution_ledgers != right.execution_ledgers {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "ordered KVM execution ledger differs".to_owned(),
                    left: json_summary(&left.execution_ledgers),
                    right: json_summary(&right.execution_ledgers),
                });
            }
            if left.machine_execution_ledgers != right.machine_execution_ledgers {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "machine-wide KVM execution stream differs".to_owned(),
                    left: json_summary(&left.machine_execution_ledgers),
                    right: json_summary(&right.machine_execution_ledgers),
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
                    });
                }
                if left.actions != right.actions {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first operation-boundary fault actions differ".to_owned(),
                        left: json_summary(&left.actions),
                        right: json_summary(&right.actions),
                    });
                }
                if left.execution_ledgers != right.execution_ledgers {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "ordered KVM execution ledger differs".to_owned(),
                        left: json_summary(&left.execution_ledgers),
                        right: json_summary(&right.execution_ledgers),
                    });
                }
                if left.machine_execution_ledgers != right.machine_execution_ledgers {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "machine-wide KVM execution stream differs".to_owned(),
                        left: json_summary(&left.machine_execution_ledgers),
                        right: json_summary(&right.machine_execution_ledgers),
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
                    });
                }
                if left.structured_choices != right.structured_choices {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first structured choice differs".to_owned(),
                        left: json_summary(&left.structured_choices),
                        right: json_summary(&right.structured_choices),
                    });
                }
                if left.thread_scheduling != right.thread_scheduling {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first thread-scheduling decision differs".to_owned(),
                        left: json_summary(&left.thread_scheduling),
                        right: json_summary(&right.thread_scheduling),
                    });
                }
                if left.thread_synchronization != right.thread_synchronization {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first thread-synchronization event differs".to_owned(),
                        left: json_summary(&left.thread_synchronization),
                        right: json_summary(&right.thread_synchronization),
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
                });
            }
            if left.property_witnesses != right.property_witnesses {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "property witnesses differ".to_owned(),
                    left: format!("{:?}", left.property_witnesses),
                    right: format!("{:?}", right.property_witnesses),
                });
            }
            if left.structured_choices != right.structured_choices {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "structured choices differ".to_owned(),
                    left: json_summary(&left.structured_choices),
                    right: json_summary(&right.structured_choices),
                });
            }
            if left.thread_scheduling != right.thread_scheduling {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "thread-scheduling decisions differ".to_owned(),
                    left: json_summary(&left.thread_scheduling),
                    right: json_summary(&right.thread_scheduling),
                });
            }
            if left.thread_synchronization != right.thread_synchronization {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "thread-synchronization events differ".to_owned(),
                    left: json_summary(&left.thread_synchronization),
                    right: json_summary(&right.thread_synchronization),
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
            })
        })
        .or_else(|| {
            (left.runs.len() != right.runs.len()).then(|| CampaignDivergence {
                run: left.runs.len().min(right.runs.len()),
                boundary: None,
                reason: "campaign run count differs".to_owned(),
                left: left.runs.len().to_string(),
                right: right.runs.len().to_string(),
            })
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
        assert_eq!(
            divergence.reason,
            "machine-wide KVM execution stream differs"
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
}
