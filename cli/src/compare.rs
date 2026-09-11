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
            (left.operations != right.operations
                || left.faults != right.faults
                || left.state_sha256 != right.state_sha256)
                .then(|| CampaignDivergence {
                    run: left.index.min(right.index).max(position),
                    reason: "first selected timeline differs".to_owned(),
                    left: format!(
                        "operations={:?}; faults={:?}; state={}",
                        left.operations, left.faults, left.state_sha256
                    ),
                    right: format!(
                        "operations={:?}; faults={:?}; state={}",
                        right.operations, right.faults, right.state_sha256
                    ),
                })
        })
        .or_else(|| {
            (left.runs.len() != right.runs.len()).then(|| CampaignDivergence {
                run: left.runs.len().min(right.runs.len()),
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
