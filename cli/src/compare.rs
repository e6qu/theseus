use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

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
}
#[derive(Deserialize)]
struct Run {
    index: usize,
    #[serde(default)]
    operations: Vec<String>,
    #[serde(default)]
    faults: Vec<String>,
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
            if left.operations != right.operations || left.faults != right.faults {
                return Some(CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "selected operation history differs".to_owned(),
                    left: format!(
                        "operations={:?}; faults={:?}; state={}",
                        left.operations, left.faults, left.state_sha256
                    ),
                    right: format!(
                        "operations={:?}; faults={:?}; state={}",
                        right.operations, right.faults, right.state_sha256
                    ),
                });
            }
            for (boundary, (left, right)) in left.timeline.iter().zip(&right.timeline).enumerate() {
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
            }
            (left.timeline.len() != right.timeline.len() || left.state_sha256 != right.state_sha256)
                .then(|| CampaignDivergence {
                    run,
                    boundary: None,
                    reason: "final topology state differs".to_owned(),
                    left: left.state_sha256.clone(),
                    right: right.state_sha256.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_the_first_different_campaign_run() {
        let left = tempfile::tempdir().unwrap();
        let right = tempfile::tempdir().unwrap();
        fs::write(left.path().join("campaign-result.json"), r#"{"runs":[{"index":0,"operations":["write"],"state_sha256":"same"},{"index":1,"operations":["read"],"state_sha256":"left"}]}"#).unwrap();
        fs::write(right.path().join("campaign-result.json"), r#"{"runs":[{"index":0,"operations":["write"],"state_sha256":"same"},{"index":1,"operations":["retry"],"state_sha256":"right"}]}"#).unwrap();
        let comparison = compare_campaigns(left.path(), right.path()).unwrap();
        assert_eq!(comparison.status, "diverged");
        assert_eq!(comparison.divergence.unwrap().run, 1);
    }
}
