// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Property verdict history across retained campaigns.
//!
//! A failed property names its first violating timeline; this module
//! connects each property's verdicts across campaigns into one auditable
//! history. The identity is the property's declaration: when the bundle's
//! replay plan retains it, the identity is a digest over that declaration,
//! so the same property declared in two campaigns joins into one history
//! while a changed needle starts a new one.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Error variants for property history retrieval.
#[derive(Debug)]
pub enum HistoryError {
    /// Cannot read a bundle file.
    Read(std::io::Error),
    /// Cannot parse a bundle file.
    Parse(serde_json::Error),
    /// A named source retains no campaign evidence.
    NotACampaign(String),
    /// No sources were named.
    NoSources,
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistoryError::Read(error) => {
                write!(formatter, "cannot read campaign bundle: {error}")
            }
            HistoryError::Parse(error) => {
                write!(formatter, "cannot parse campaign bundle: {error}")
            }
            HistoryError::NotACampaign(reason) => write!(formatter, "{reason}"),
            HistoryError::NoSources => {
                write!(
                    formatter,
                    "property history needs at least one campaign directory"
                )
            }
        }
    }
}

impl std::error::Error for HistoryError {}

impl From<std::io::Error> for HistoryError {
    fn from(error: std::io::Error) -> Self {
        HistoryError::Read(error)
    }
}

impl From<serde_json::Error> for HistoryError {
    fn from(error: serde_json::Error) -> Self {
        HistoryError::Parse(error)
    }
}

/// One verdict of one property in one retained campaign.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PropertyVerdictRecord {
    /// The canonicalized campaign directory.
    pub source: String,
    /// The property's retained verdict.
    pub status: String,
    /// The campaign's overall retained status.
    pub campaign_status: String,
    pub run_count: usize,
    /// Indices of the campaign's failed timelines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_runs: Vec<usize>,
    /// The retained verdict detail, verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// One property's verdict history across the named campaigns.
#[derive(Debug, Serialize)]
pub struct PropertyHistoryEntry {
    pub name: String,
    pub kind: String,
    /// The digest of the property's declaration in the retained replay
    /// plan, when the bundle retains one. The same declaration in two
    /// campaigns shares a digest; a changed declaration starts a new
    /// history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration_sha256: Option<String>,
    /// Verdicts in the order the campaigns were named.
    pub verdicts: Vec<PropertyVerdictRecord>,
    /// The first named campaign that retained this property as failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_failed_source: Option<String>,
}

/// The versioned property history over the named campaigns.
#[derive(Debug, Serialize)]
pub struct CampaignPropertyHistory {
    pub format: &'static str,
    /// The canonicalized campaign directories, in the order named.
    pub sources: Vec<String>,
    pub properties: Vec<PropertyHistoryEntry>,
}

fn read_json(path: &Path) -> Result<Option<serde_json::Value>, HistoryError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(HistoryError::Read(error)),
    }
}

/// The declaration digest for one property name from a bundle's replay
/// plan, when the plan retains the campaign's property declarations. The
/// digest covers the canonical serialization of the whole declaration, so
/// any change to the needle, predicate, or guards starts a new history.
fn declaration_digest(plan: &serde_json::Value, name: &str) -> Option<String> {
    let declaration = plan["campaign"]["properties"]
        .as_array()?
        .iter()
        .find(|property| property["name"] == *name)?;
    let canonical = serde_json::to_vec(declaration).ok()?;
    Some(format!("{:x}", Sha256::digest(&canonical)))
}

/// Build the property verdict history over the named campaign directories.
/// Verdicts group by declaration identity; the campaigns are read in the
/// order they are named, and every named directory must retain a campaign
/// result.
pub fn property_history(
    sources: &[std::path::PathBuf],
    property_filter: Option<&str>,
) -> Result<CampaignPropertyHistory, HistoryError> {
    if sources.is_empty() {
        return Err(HistoryError::NoSources);
    }
    let mut canonical_sources = Vec::with_capacity(sources.len());
    let mut grouped: BTreeMap<(String, String, Option<String>), PropertyHistoryEntry> =
        BTreeMap::new();
    for source in sources {
        let bundle = fs::canonicalize(source).map_err(HistoryError::Read)?;
        let result = read_json(&bundle.join("campaign-result.json"))?;
        let Some(result) = result else {
            return Err(HistoryError::NotACampaign(format!(
                "no campaign-result.json in {}",
                bundle.display()
            )));
        };
        let plan = read_json(&bundle.join("replay-plan.json"))?;
        let campaign_status = result["status"].as_str().unwrap_or("unknown").to_owned();
        let run_count = result["runs"]
            .as_array()
            .map(|runs| runs.len())
            .unwrap_or_default();
        let failed_runs = result["runs"]
            .as_array()
            .map(|runs| {
                runs.iter()
                    .filter(|run| run["status"] == "failed")
                    .filter_map(|run| run["index"].as_u64())
                    .map(|index| index as usize)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        canonical_sources.push(bundle.display().to_string());

        for verdict in result["properties"].as_array().unwrap_or(&Vec::new()) {
            let name = verdict["name"].as_str().unwrap_or_default().to_owned();
            if let Some(filter) = property_filter {
                if name != filter {
                    continue;
                }
            }
            let kind = verdict["kind"].as_str().unwrap_or_default().to_owned();
            let declaration_sha256 = plan
                .as_ref()
                .and_then(|plan| declaration_digest(plan, &name));
            let key = (name.clone(), kind.clone(), declaration_sha256.clone());
            let entry = grouped.entry(key).or_insert_with(|| PropertyHistoryEntry {
                name: name.clone(),
                kind,
                declaration_sha256,
                verdicts: Vec::new(),
                first_failed_source: None,
            });
            let status = verdict["status"].as_str().unwrap_or_default().to_owned();
            if status == "failed" && entry.first_failed_source.is_none() {
                entry.first_failed_source = Some(bundle.display().to_string());
            }
            entry.verdicts.push(PropertyVerdictRecord {
                source: bundle.display().to_string(),
                status,
                campaign_status: campaign_status.clone(),
                run_count,
                failed_runs: failed_runs.clone(),
                detail: verdict["detail"].as_str().unwrap_or_default().to_owned(),
            });
        }
    }
    Ok(CampaignPropertyHistory {
        format: "theseus-campaign-property-history-v1",
        sources: canonical_sources,
        properties: grouped.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PLAN: &str = r#"{"format":"theseus-compose-plan-v1","campaign":{"driver":"api","properties":[
        {"name":"lost_update","kind":"always","contains":"THES:ASSERT:no_data_loss:pass"}
    ]}}"#;

    fn write_bundle(directory: &Path, result: &str, plan: Option<&str>) -> PathBuf {
        fs::create_dir_all(directory).unwrap();
        fs::write(directory.join("campaign-result.json"), result).unwrap();
        if let Some(plan) = plan {
            fs::write(directory.join("replay-plan.json"), plan).unwrap();
        }
        directory.to_path_buf()
    }

    #[test]
    fn history_joins_the_same_declaration_and_separates_changed_ones() {
        let directory = tempfile::tempdir().unwrap();
        let before = write_bundle(
            &directory.path().join("before"),
            r#"{"status":"failed","runs":[{"index":0,"status":"passed"},{"index":1,"status":"failed"}],
               "properties":[{"name":"lost_update","kind":"always","status":"failed","detail":"0 of 2 retained timelines satisfied the serial needle"}]}"#,
            Some(PLAN),
        );
        let after = write_bundle(
            &directory.path().join("after"),
            r#"{"status":"passed","runs":[{"index":0,"status":"passed"}],
               "properties":[{"name":"lost_update","kind":"always","status":"passed","detail":"1 of 1 retained timelines satisfied the serial needle"}]}"#,
            Some(PLAN),
        );
        // The same name but a different declaration is a different history.
        let changed_plan = PLAN.replace("no_data_loss", "no_stale_reads");
        let changed = write_bundle(
            &directory.path().join("changed"),
            r#"{"status":"passed","runs":[{"index":0,"status":"passed"}],
               "properties":[{"name":"lost_update","kind":"always","status":"passed","detail":"changed needle"}]}"#,
            Some(&changed_plan),
        );

        let history =
            property_history(&[before.clone(), after.clone(), changed.clone()], None).unwrap();
        assert_eq!(history.format, "theseus-campaign-property-history-v1");
        assert_eq!(history.sources.len(), 3);
        assert_eq!(history.properties.len(), 2);

        let shared = history
            .properties
            .iter()
            .find(|entry| entry.verdicts.len() == 2)
            .expect("the unchanged declarations share one history");
        assert_eq!(shared.name, "lost_update");
        let canonical_before = fs::canonicalize(&before).unwrap();
        assert_eq!(
            shared.first_failed_source.as_deref(),
            Some(canonical_before.to_str().unwrap())
        );
        assert_eq!(
            shared.verdicts[0].source,
            canonical_before.to_str().unwrap()
        );
        assert_eq!(shared.verdicts[0].status, "failed");
        assert_eq!(shared.verdicts[0].campaign_status, "failed");
        assert_eq!(shared.verdicts[0].failed_runs, vec![1]);
        assert_eq!(shared.verdicts[1].status, "passed");
        let digest = shared.declaration_sha256.as_deref().unwrap();
        assert_eq!(digest.len(), 64);

        let changed_entry = history
            .properties
            .iter()
            .find(|entry| entry.verdicts.len() == 1)
            .unwrap();
        assert_ne!(changed_entry.declaration_sha256.as_deref(), Some(digest));
        assert!(changed_entry.first_failed_source.is_none());
    }

    #[test]
    fn history_without_plans_groups_by_name_and_filters_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let first = write_bundle(
            &directory.path().join("first"),
            r#"{"status":"passed","runs":[{"index":0,"status":"passed"}],
               "properties":[{"name":"recovery","kind":"reachable","status":"passed","detail":""}]}"#,
            None,
        );
        let second = write_bundle(
            &directory.path().join("second"),
            r#"{"status":"passed","runs":[{"index":0,"status":"passed"}],
               "properties":[{"name":"recovery","kind":"reachable","status":"passed","detail":""}]}"#,
            None,
        );
        let history = property_history(&[first, second], Some("recovery")).unwrap();
        assert_eq!(history.properties.len(), 1);
        assert_eq!(history.properties[0].verdicts.len(), 2);
        assert_eq!(history.properties[0].declaration_sha256, None);

        let filtered = property_history(&[directory.path().join("first")], Some("absent")).unwrap();
        assert!(filtered.properties.is_empty());
    }

    #[test]
    fn history_requires_sources_and_campaign_results() {
        assert!(property_history(&[], None).is_err());

        let directory = tempfile::tempdir().unwrap();
        let error = property_history(&[directory.path().to_path_buf()], None).unwrap_err();
        assert!(
            error.to_string().contains("no campaign-result.json"),
            "{error}"
        );
    }
}
