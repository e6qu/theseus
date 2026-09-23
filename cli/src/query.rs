// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Moment-scoped retrieval over a retained campaign bundle.
//!
//! Campaign results address every operation boundary with a moment —
//! `<vtime_ns>@<input_sha256>` for the service that received the operation.
//! This module resolves an address to its boundary and exposes temporal
//! navigation to the preceding and following moments, so a divergence
//! report's address resolves to log text offline.

use std::fmt;
use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::ComposeError;

/// One resolved moment: the boundary an address identifies, with its log
/// excerpts and the neighboring addresses for temporal navigation.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MomentHit {
    /// Index of the retained timeline (0-based).
    pub run: usize,
    /// `op-NNN-<operation>` boundary identity.
    pub boundary: String,
    /// The service that received the operation.
    pub service: String,
    /// The operation name.
    pub operation: String,
    /// Cumulative virtual time in nanoseconds at this boundary.
    pub vtime_ns: u64,
    /// The operation input digest.
    pub input_sha256: String,
    /// Bounded serial excerpt per service, verbatim from the result.
    pub excerpts: Vec<(String, String)>,
    /// The preceding boundary's moment address, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// The following boundary's moment address, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Error variants for moment retrieval.
#[derive(Debug)]
pub enum MomentError {
    /// Cannot read the campaign result.
    Read(std::io::Error),
    /// Cannot parse the campaign result.
    Parse(serde_json::Error),
    /// The moment address did not resolve.
    NotFound(String),
    /// The bundle plan could not be loaded.
    Compose(ComposeError),
}

impl fmt::Display for MomentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MomentError::Read(error) => write!(formatter, "cannot read campaign result: {error}"),
            MomentError::Parse(error) => {
                write!(formatter, "cannot parse campaign result: {error}")
            }
            MomentError::NotFound(reason) => write!(formatter, "{reason}"),
            MomentError::Compose(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for MomentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MomentError::Read(error) => Some(error),
            MomentError::Parse(error) => Some(error),
            MomentError::Compose(error) => Some(error),
            MomentError::NotFound(_) => None,
        }
    }
}

impl From<std::io::Error> for MomentError {
    fn from(error: std::io::Error) -> Self {
        MomentError::Read(error)
    }
}

impl From<serde_json::Error> for MomentError {
    fn from(error: serde_json::Error) -> Self {
        MomentError::Parse(error)
    }
}

impl From<ComposeError> for MomentError {
    fn from(error: ComposeError) -> Self {
        MomentError::Compose(error)
    }
}

/// Resolve one moment address inside a retained campaign result.
pub fn find_moment(result: &serde_json::Value, moment: &str) -> Result<MomentHit, MomentError> {
    let runs = result["runs"]
        .as_array()
        .ok_or_else(|| MomentError::NotFound("result has no runs".to_owned()))?;
    for (run_index, run) in runs.iter().enumerate() {
        let timeline = run["timeline"]
            .as_array()
            .ok_or_else(|| MomentError::NotFound(format!("run {run_index} has no timeline")))?;
        for (boundary_index, boundary) in timeline.iter().enumerate() {
            if boundary["moment"] == *moment {
                let service = boundary["service"].as_str().unwrap_or_default().to_owned();
                let (vtime_text, input_sha256) = moment
                    .split_once('@')
                    .map(|(vtime, hash)| (vtime.to_owned(), hash.to_owned()))
                    .unzip();
                let vtime_ns = vtime_text.unwrap_or_default().parse().unwrap_or_default();
                let mut excerpts = Vec::new();
                if let Some(deltas) = boundary["serial_delta"].as_object() {
                    for (service, delta) in deltas {
                        let excerpt = delta["excerpt"].as_str().unwrap_or_default();
                        excerpts.push((service.clone(), excerpt.to_owned()));
                    }
                }
                let previous = timeline
                    .get(boundary_index.wrapping_sub(1))
                    .filter(|_| boundary_index > 0)
                    .and_then(|boundary| boundary["moment"].as_str())
                    .map(str::to_owned);
                let next = timeline
                    .get(boundary_index + 1)
                    .and_then(|boundary| boundary["moment"].as_str())
                    .map(str::to_owned);
                return Ok(MomentHit {
                    run: run_index,
                    boundary: boundary["id"].as_str().unwrap_or_default().to_owned(),
                    service,
                    operation: boundary["operation"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    vtime_ns,
                    input_sha256: input_sha256.unwrap_or_default(),
                    excerpts,
                    previous,
                    next,
                });
            }
        }
    }
    Err(MomentError::NotFound(format!(
        "no boundary carries moment {moment:?}"
    )))
}

/// Load the campaign result from a bundle directory and resolve the moment.
pub fn query_moment(bundle: impl AsRef<Path>, moment: &str) -> Result<MomentHit, MomentError> {
    let path = bundle.as_ref().join("campaign-result.json");
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    find_moment(&result, moment)
}

/// One row of the full moment index over a retained campaign.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MomentSummary {
    /// Index of the retained timeline (0-based).
    pub run: usize,
    /// `op-NNN-<operation>` boundary identity.
    pub boundary: String,
    /// The service that received the operation.
    pub service: String,
    /// The operation name.
    pub operation: String,
    /// The moment address.
    pub moment: String,
}

/// List every moment address in a retained campaign, in timeline order.
/// A `service` filter narrows the index to boundaries that service
/// received.
pub fn list_moments(
    result: &serde_json::Value,
    service: Option<&str>,
) -> Result<Vec<MomentSummary>, MomentError> {
    let runs = result["runs"]
        .as_array()
        .ok_or_else(|| MomentError::NotFound("result has no runs".to_owned()))?;
    let mut summaries = Vec::new();
    for (run_index, run) in runs.iter().enumerate() {
        let timeline = run["timeline"]
            .as_array()
            .ok_or_else(|| MomentError::NotFound(format!("run {run_index} has no timeline")))?;
        for boundary in timeline {
            let boundary_service = boundary["service"].as_str().unwrap_or_default().to_owned();
            if let Some(filter) = service {
                if boundary_service != filter {
                    continue;
                }
            }
            summaries.push(MomentSummary {
                run: run_index,
                boundary: boundary["id"].as_str().unwrap_or_default().to_owned(),
                service: boundary_service,
                operation: boundary["operation"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                moment: boundary["moment"].as_str().unwrap_or_default().to_owned(),
            });
        }
    }
    Ok(summaries)
}

/// Resolve the moment immediately following `moment` in the same timeline.
pub fn next_moment_in(result: &serde_json::Value, moment: &str) -> Result<MomentHit, MomentError> {
    let hit = find_moment(result, moment)?;
    let next = hit.next.ok_or_else(|| {
        MomentError::NotFound(format!("moment {moment:?} has no following moment"))
    })?;
    find_moment(result, &next)
}

/// Resolve the moment immediately preceding `moment` in the same timeline.
pub fn previous_moment_in(
    result: &serde_json::Value,
    moment: &str,
) -> Result<MomentHit, MomentError> {
    let hit = find_moment(result, moment)?;
    let previous = hit.previous.ok_or_else(|| {
        MomentError::NotFound(format!("moment {moment:?} has no preceding moment"))
    })?;
    find_moment(result, &previous)
}

/// Load a bundle's campaign result and resolve the following moment.
pub fn next_moment(bundle: impl AsRef<Path>, moment: &str) -> Result<MomentHit, MomentError> {
    let path = bundle.as_ref().join("campaign-result.json");
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    next_moment_in(&result, moment)
}

/// Load a bundle's campaign result and resolve the preceding moment.
pub fn previous_moment(bundle: impl AsRef<Path>, moment: &str) -> Result<MomentHit, MomentError> {
    let path = bundle.as_ref().join("campaign-result.json");
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    previous_moment_in(&result, moment)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::json!({
            "runs": [
                {"index": 0, "timeline": [
                    {"id": "op-000-write", "operation": "write", "service": "api",
                     "moment": "7000@input-hash",
                     "serial_delta": {"api": {"bytes": 16, "sha256": "write-hash", "excerpt": "write\ncomplete\n", "omitted_bytes": 0}}},
                    {"id": "op-001-read", "operation": "read", "service": "counter",
                     "moment": "9000@read-hash",
                     "serial_delta": {"counter": {"bytes": 11, "sha256": "read-hash", "excerpt": "ready\n", "omitted_bytes": 2}}}
                ]}
            ]
        })
    }

    #[test]
    fn moments_resolve_to_boundaries_with_neighbors_and_excerpts() {
        let result = fixture();
        let hit = find_moment(&result, "7000@input-hash").unwrap();
        assert_eq!(hit.run, 0);
        assert_eq!(hit.boundary, "op-000-write");
        assert_eq!(hit.operation, "write");
        assert_eq!(hit.service, "api");
        assert_eq!(hit.vtime_ns, 7000);
        assert_eq!(hit.input_sha256, "input-hash");
        assert_eq!(hit.previous, None);
        assert_eq!(hit.next, Some("9000@read-hash".to_owned()));
        assert_eq!(
            hit.excerpts,
            vec![("api".to_owned(), "write\ncomplete\n".to_owned())]
        );

        let second = find_moment(&result, "9000@read-hash").unwrap();
        assert_eq!(second.previous, Some("7000@input-hash".to_owned()));
        assert_eq!(second.next, None);
        assert_eq!(second.vtime_ns, 9000);
    }

    #[test]
    fn navigation_walks_neighbors_and_enumeration_lists_every_moment() {
        let result = fixture();
        let next = next_moment_in(&result, "7000@input-hash").unwrap();
        assert_eq!(next.boundary, "op-001-read");
        let previous = previous_moment_in(&result, "9000@read-hash").unwrap();
        assert_eq!(previous.boundary, "op-000-write");
        assert!(next_moment_in(&result, "9000@read-hash")
            .unwrap_err()
            .to_string()
            .contains("no following moment"));
        assert!(previous_moment_in(&result, "7000@input-hash")
            .unwrap_err()
            .to_string()
            .contains("no preceding moment"));

        let summaries = list_moments(&result, None).unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].moment, "7000@input-hash");
        assert_eq!(summaries[0].operation, "write");
        assert_eq!(summaries[1].service, "counter");
        assert_eq!(summaries[1].moment, "9000@read-hash");

        let filtered = list_moments(&result, Some("counter")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].service, "counter");
        assert_eq!(filtered[0].moment, "9000@read-hash");
    }

    #[test]
    fn unknown_moments_fail_with_the_address() {
        let error = find_moment(&fixture(), "1@2").unwrap_err();
        assert!(error.to_string().contains("1@2"), "{error}");
    }
}
