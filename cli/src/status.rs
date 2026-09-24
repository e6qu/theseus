// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! One versioned answer to "what did this campaign conclude".
//!
//! Notifications, CI gating, and issue bots read this summary instead of
//! parsing result internals: the retained campaign status, its failed runs
//! and properties, the declared policy, and the retained artifact inventory,
//! in one stable JSON shape over old and new bundles.

use std::fmt;
use std::fs;
use std::path::Path;

use serde::Serialize;

/// Error variants for campaign status retrieval.
#[derive(Debug)]
pub enum StatusError {
    /// Cannot read a bundle file.
    Read(std::io::Error),
    /// Cannot parse a bundle file.
    Parse(serde_json::Error),
    /// The directory retains no campaign evidence.
    NotACampaign(String),
}

impl fmt::Display for StatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatusError::Read(error) => write!(formatter, "cannot read campaign bundle: {error}"),
            StatusError::Parse(error) => {
                write!(formatter, "cannot parse campaign bundle: {error}")
            }
            StatusError::NotACampaign(reason) => write!(formatter, "{reason}"),
        }
    }
}

impl std::error::Error for StatusError {}

impl From<std::io::Error> for StatusError {
    fn from(error: std::io::Error) -> Self {
        StatusError::Read(error)
    }
}

impl From<serde_json::Error> for StatusError {
    fn from(error: serde_json::Error) -> Self {
        StatusError::Parse(error)
    }
}

/// One retained property verdict, verbatim from the campaign result.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PropertyVerdict {
    pub name: String,
    pub kind: String,
    pub status: String,
    /// The retained detail, including the first-violation context for
    /// failed verdicts.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// Which retained artifacts the bundle still carries.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ArtifactInventory {
    /// `campaign-result.json` is present.
    pub result: bool,
    /// `replay-plan.json` is present.
    pub plan: bool,
    /// The number of retained `runs/NNN` timelines.
    pub runs: usize,
    /// The shared ready checkpoint is present.
    pub checkpoint: bool,
}

/// The versioned summary of one retained campaign.
#[derive(Debug, Serialize)]
pub struct CampaignStatus {
    pub format: &'static str,
    /// The canonicalized campaign directory.
    pub source: String,
    /// The retained result format, when a campaign result exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign_format: Option<String>,
    /// `passed`, `failed`, or `counterexample` for minimized exports.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
    /// The declared run budget, from the retained replay plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<u16>,
    pub run_count: usize,
    /// Indices of retained timelines whose status is `failed`.
    pub failed_runs: Vec<usize>,
    /// Names of every property retained as failed.
    pub failed_properties: Vec<String>,
    pub properties: Vec<PropertyVerdict>,
    /// The minimized counterexample a bundle retains, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterexample: Option<String>,
    pub artifacts: ArtifactInventory,
}

fn optional_text(value: &serde_json::Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>, StatusError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StatusError::Read(error)),
    }
}

/// Summarize one retained campaign bundle: the retained status, failed runs
/// and properties, the declared policy, and which artifacts remain. Works
/// over current campaign results, old bundles with missing fields, and
/// minimized counterexample exports.
pub fn campaign_status(bundle: impl AsRef<Path>) -> Result<CampaignStatus, StatusError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(StatusError::Read)?;
    let result = read_json(&bundle.join("campaign-result.json"))?;
    let plan = read_json(&bundle.join("replay-plan.json"))?;
    let minimization = read_json(&bundle.join("minimization.json"))?;
    let Some(result) = result else {
        if let Some(minimization) = &minimization {
            let property = minimization["property"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned();
            return Ok(CampaignStatus {
                format: "theseus-campaign-status-v1",
                source: bundle.display().to_string(),
                campaign_format: None,
                status: "counterexample".to_owned(),
                driver: plan
                    .as_ref()
                    .and_then(|plan| plan["campaign"]["driver"].as_str())
                    .map(str::to_owned),
                guidance: None,
                coverage: None,
                budget: plan.as_ref().and_then(|plan| {
                    plan["campaign"]["max_runs"]
                        .as_u64()
                        .map(|budget| budget as u16)
                }),
                run_count: 0,
                failed_runs: Vec::new(),
                failed_properties: vec![property.clone()],
                properties: Vec::new(),
                counterexample: Some(property),
                artifacts: artifact_inventory(&bundle),
            });
        }
        return Err(StatusError::NotACampaign(format!(
            "no campaign-result.json or minimization.json in {}",
            bundle.display()
        )));
    };

    let runs = result["runs"].as_array();
    let run_count = runs.map(|runs| runs.len()).unwrap_or_default();
    let failed_runs = runs
        .map(|runs| {
            runs.iter()
                .filter(|run| run["status"] == "failed")
                .filter_map(|run| run["index"].as_u64())
                .map(|index| index as usize)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let properties: Vec<PropertyVerdict> = result["properties"]
        .as_array()
        .map(|properties| {
            properties
                .iter()
                .map(|property| PropertyVerdict {
                    name: property["name"].as_str().unwrap_or_default().to_owned(),
                    kind: property["kind"].as_str().unwrap_or_default().to_owned(),
                    status: property["status"].as_str().unwrap_or_default().to_owned(),
                    detail: property["detail"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let failed_properties = properties
        .iter()
        .filter(|property| property.status == "failed")
        .map(|property| property.name.clone())
        .collect::<Vec<_>>();

    Ok(CampaignStatus {
        format: "theseus-campaign-status-v1",
        source: bundle.display().to_string(),
        campaign_format: optional_text(&result, "format"),
        status: optional_text(&result, "status").unwrap_or_else(|| "unknown".to_owned()),
        driver: optional_text(&result, "driver"),
        guidance: optional_text(&result, "guidance"),
        coverage: optional_text(&result, "coverage"),
        budget: plan.as_ref().and_then(|plan| {
            plan["campaign"]["max_runs"]
                .as_u64()
                .map(|budget| budget as u16)
        }),
        run_count,
        failed_runs,
        failed_properties,
        properties,
        counterexample: minimization
            .as_ref()
            .and_then(|minimization| minimization["property"].as_str())
            .map(str::to_owned),
        artifacts: artifact_inventory(&bundle),
    })
}

fn artifact_inventory(bundle: &Path) -> ArtifactInventory {
    ArtifactInventory {
        result: bundle.join("campaign-result.json").is_file(),
        plan: bundle.join("replay-plan.json").is_file(),
        runs: fs::read_dir(bundle.join("runs"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().is_dir())
                    .count()
            })
            .unwrap_or_default(),
        checkpoint: bundle.join("checkpoint").is_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_bundle(
        directory: impl AsRef<Path>,
        result: Option<&str>,
        plan: Option<&str>,
    ) -> PathBuf {
        let directory = directory.as_ref();
        fs::create_dir_all(directory).unwrap();
        if let Some(result) = result {
            fs::write(directory.join("campaign-result.json"), result).unwrap();
        }
        if let Some(plan) = plan {
            fs::write(directory.join("replay-plan.json"), plan).unwrap();
        }
        directory.to_path_buf()
    }

    #[test]
    fn status_summarizes_a_failed_campaign_with_its_verdicts_and_policy() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = write_bundle(
            directory.path(),
            Some(
                r#"{"format":"theseus-compose-campaign-result-v1","status":"failed",
                    "driver":"api","guidance":"unified","coverage":"execution_locations",
                    "runs":[
                        {"index":0,"status":"passed"},
                        {"index":1,"status":"failed"}
                    ],
                    "properties":[
                        {"name":"lost_update","kind":"always","status":"failed","detail":"0 of 2 retained timelines satisfied the serial needle — first violation: service counter: stale"},
                        {"name":"recovery","kind":"reachable","status":"passed","detail":"1 of 2 retained timelines satisfied the recovery marker"}
                    ]}"#,
            ),
            Some(
                r#"{"format":"theseus-compose-plan-v1","campaign":{"driver":"api","max_runs":64}}"#,
            ),
        );
        fs::create_dir_all(bundle.join("runs/000")).unwrap();
        fs::create_dir_all(bundle.join("runs/001")).unwrap();
        fs::create_dir_all(bundle.join("checkpoint")).unwrap();

        let status = campaign_status(&bundle).unwrap();
        assert_eq!(status.format, "theseus-campaign-status-v1");
        assert_eq!(status.status, "failed");
        assert_eq!(
            status.campaign_format.as_deref(),
            Some("theseus-compose-campaign-result-v1")
        );
        assert_eq!(status.driver.as_deref(), Some("api"));
        assert_eq!(status.guidance.as_deref(), Some("unified"));
        assert_eq!(status.budget, Some(64));
        assert_eq!(status.run_count, 2);
        assert_eq!(status.failed_runs, vec![1]);
        assert_eq!(status.failed_properties, vec!["lost_update".to_owned()]);
        assert_eq!(status.properties.len(), 2);
        assert_eq!(status.properties[0].name, "lost_update");
        assert!(status.properties[0].detail.contains("first violation"));
        assert_eq!(status.counterexample, None);
        assert!(status.artifacts.result);
        assert!(status.artifacts.plan);
        assert_eq!(status.artifacts.runs, 2);
        assert!(status.artifacts.checkpoint);
    }

    #[test]
    fn status_tolerates_old_bundles_and_summarizes_counterexample_exports() {
        let directory = tempfile::tempdir().unwrap();

        // An old result with no policy, coverage, or detail fields.
        let bundle = write_bundle(
            directory.path().join("old"),
            Some(r#"{"status":"passed","runs":[{"index":0,"status":"passed"}]}"#),
            None,
        );
        let status = campaign_status(&bundle).unwrap();
        assert_eq!(status.status, "passed");
        assert_eq!(status.campaign_format, None);
        assert_eq!(status.guidance, None);
        assert_eq!(status.budget, None);
        assert_eq!(status.failed_properties, Vec::<String>::new());
        assert!(status.properties.is_empty());
        assert!(status.artifacts.result);
        assert!(!status.artifacts.plan);

        // A minimized counterexample export retains no campaign result.
        let bundle = directory.path().join("minimized");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(
            bundle.join("minimization.json"),
            r#"{"property":"lost_update"}"#,
        )
        .unwrap();
        fs::write(bundle.join("topology-result.json"), "{}").unwrap();
        let status = campaign_status(&bundle).unwrap();
        assert_eq!(status.status, "counterexample");
        assert_eq!(status.counterexample.as_deref(), Some("lost_update"));
        assert_eq!(status.failed_properties, vec!["lost_update".to_owned()]);
        assert_eq!(status.run_count, 0);
    }

    #[test]
    fn status_rejects_directories_without_campaign_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let error = campaign_status(directory.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no campaign-result.json or minimization.json"),
            "{error}"
        );
    }
}
