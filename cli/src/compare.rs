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
    state_sha256: String,
    #[serde(default)]
    timeline: Vec<Boundary>,
}
#[derive(Deserialize)]
struct Boundary {
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
                report.push_str("\n## First causal divergence\n\n");
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
            for (boundary, (left, right)) in left.timeline.iter().zip(&right.timeline).enumerate() {
                if left.actions != right.actions {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first operation-boundary fault actions differ".to_owned(),
                        left: json_summary(&left.actions),
                        right: json_summary(&right.actions),
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
                {
                    return Some(CampaignDivergence {
                        run,
                        boundary: Some(boundary),
                        reason: "first operation-boundary coverage differs".to_owned(),
                        left: format!(
                            "program_counters={}; instruction_locations={}",
                            json_summary(&left.program_counters),
                            json_summary(&left.instruction_locations)
                        ),
                        right: format!(
                            "program_counters={}; instruction_locations={}",
                            json_summary(&right.program_counters),
                            json_summary(&right.instruction_locations)
                        ),
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
            if left.program_counters != right.program_counters
                || left.instruction_locations != right.instruction_locations
                || left.instruction_novelty != right.instruction_novelty
                || left.checkpoint_pc_novelty != right.checkpoint_pc_novelty
            {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "accumulated campaign coverage differs".to_owned(),
                    left: coverage_summary(left),
                    right: coverage_summary(right),
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
        "program_counters={}; instruction_locations={}; instruction_novelty={:?}; checkpoint_pc_novelty={:?}",
        json_summary(&run.program_counters),
        json_summary(&run.instruction_locations),
        run.instruction_novelty,
        run.checkpoint_pc_novelty,
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
        assert!(comparison.markdown().contains("First causal divergence"));

        let query = query_campaigns(left.path(), right.path(), "/properties/0/status").unwrap();
        assert!(!query.equal);
        assert_eq!(query.left, Some(Value::String("passed".to_owned())));
        assert_eq!(query.right, Some(Value::String("failed".to_owned())));
    }
}
