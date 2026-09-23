// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Moment-scoped retrieval over a retained campaign bundle.
//!
//! Campaign results address every operation boundary with a moment —
//! `<vtime_ns>@<input_sha256>` for the service that received the operation.
//! This module resolves an address to its boundary and exposes temporal
//! navigation to the preceding and following moments, so a divergence
//! report's address resolves to log text offline.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

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

/// The temporal relation a query evaluates over the retained moment space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalRelation {
    /// The needle's serial evidence occurs strictly before the moment,
    /// mirroring the property layer's `requires_serial_*` guard semantics:
    /// what the service had already printed when the boundary was taken.
    PrecededBy,
    /// The needle's serial evidence occurs strictly after the moment.
    FollowedBy,
}

impl TemporalRelation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PrecededBy => "preceded_by",
            Self::FollowedBy => "followed_by",
        }
    }
}

/// One place the queried needle printed: the boundary whose retained serial
/// delta contains it, and the service that printed it. The boundary's
/// receiving service may differ from the printing service.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct NeedleOccurrence {
    /// Index of the retained timeline (0-based).
    pub run: usize,
    /// `op-NNN-<operation>` boundary identity.
    pub boundary: String,
    /// The operation name.
    pub operation: String,
    /// The service whose serial delta contains the needle.
    pub service: String,
    /// The moment address of the boundary where the needle printed.
    pub moment: String,
}

/// The answer to one temporal query over a retained campaign: where the
/// needle printed, and every moment satisfying the relation. Relations are
/// evaluated inside each run's timeline, over the same bounded serial
/// excerpts the campaign report's moment log shows.
#[derive(Debug, Serialize)]
pub struct TemporalQuery {
    pub format: &'static str,
    pub relation: &'static str,
    /// The needle as matched: ASCII-escaped exactly like the retained
    /// excerpts, so the query answers over the retained bytes verbatim.
    pub needle: String,
    /// The service scope filter, echoed back when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Every boundary whose serial delta contains the needle.
    pub occurrences: Vec<NeedleOccurrence>,
    /// Every moment satisfying the relation against those occurrences.
    pub matches: Vec<MomentSummary>,
}

/// Evaluate a `preceded-by`/`followed-by` relation between one needle and
/// every retained moment. The needle matches a boundary when it occurs in
/// that boundary's retained serial delta (for the scoped service, or any
/// service); a moment then matches `preceded_by` when some occurrence lies
/// strictly before it in the same run, and `followed_by` when some
/// occurrence lies strictly after it.
pub fn temporal_query(
    result: &serde_json::Value,
    relation: TemporalRelation,
    needle: &str,
    service: Option<&str>,
) -> Result<TemporalQuery, MomentError> {
    if needle.is_empty() {
        return Err(MomentError::NotFound(
            "temporal query requires a non-empty needle".to_owned(),
        ));
    }
    let escaped_needle = String::from_utf8(
        needle
            .bytes()
            .flat_map(std::ascii::escape_default)
            .collect::<Vec<u8>>(),
    )
    .expect("escaped needles are ASCII");
    let runs = result["runs"]
        .as_array()
        .ok_or_else(|| MomentError::NotFound("result has no runs".to_owned()))?;
    let mut occurrences = Vec::new();
    let mut matches = Vec::new();
    for (run_index, run) in runs.iter().enumerate() {
        let timeline = run["timeline"]
            .as_array()
            .ok_or_else(|| MomentError::NotFound(format!("run {run_index} has no timeline")))?;
        // The timeline indices whose serial delta contains the needle.
        let mut occurrence_indices = Vec::new();
        for (boundary_index, boundary) in timeline.iter().enumerate() {
            let mut delta_matches = false;
            if let Some(deltas) = boundary["serial_delta"].as_object() {
                for (delta_service, delta) in deltas {
                    if let Some(filter) = service {
                        if delta_service != filter {
                            continue;
                        }
                    }
                    let excerpt = delta["excerpt"].as_str().unwrap_or_default();
                    if excerpt.contains(escaped_needle.as_str()) {
                        delta_matches = true;
                        occurrences.push(NeedleOccurrence {
                            run: run_index,
                            boundary: boundary["id"].as_str().unwrap_or_default().to_owned(),
                            operation: boundary["operation"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                            service: delta_service.clone(),
                            moment: boundary["moment"].as_str().unwrap_or_default().to_owned(),
                        });
                    }
                }
            }
            if delta_matches {
                occurrence_indices.push(boundary_index);
            }
        }
        for (boundary_index, boundary) in timeline.iter().enumerate() {
            let related = match relation {
                TemporalRelation::PrecededBy => occurrence_indices
                    .iter()
                    .any(|occurrence| *occurrence < boundary_index),
                TemporalRelation::FollowedBy => occurrence_indices
                    .iter()
                    .any(|occurrence| *occurrence > boundary_index),
            };
            if !related {
                continue;
            }
            let boundary_service = boundary["service"].as_str().unwrap_or_default().to_owned();
            if let Some(filter) = service {
                if boundary_service != filter {
                    continue;
                }
            }
            matches.push(MomentSummary {
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
    Ok(TemporalQuery {
        format: "theseus-query-temporal-v1",
        relation: relation.as_str(),
        needle: escaped_needle,
        service: service.map(str::to_owned),
        occurrences,
        matches,
    })
}

/// Load a bundle's campaign result and evaluate the temporal query.
pub fn query_temporal(
    bundle: impl AsRef<Path>,
    relation: TemporalRelation,
    needle: &str,
    service: Option<&str>,
) -> Result<TemporalQuery, MomentError> {
    let path = bundle.as_ref().join("campaign-result.json");
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    temporal_query(&result, relation, needle, service)
}

/// One file inside a collected artifact bundle, with the digest that makes
/// the bundle auditable without the original campaign.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CollectedFile {
    /// Path relative to the collected directory.
    pub path: String,
    pub sha256: String,
}

/// The manifest of one moment collected into a self-contained artifact
/// bundle: the boundary's full record with its neighbors, the decision-trace
/// slice that produced it, serial-log slices when the retained run directory
/// is available, and the digest of every collected file.
#[derive(Debug, Serialize)]
pub struct CollectedMoment {
    pub format: &'static str,
    /// The canonicalized source bundle directory.
    pub source: String,
    pub run: usize,
    /// The moment address the collection was scoped to.
    pub moment: String,
    pub boundary: String,
    pub operation: String,
    /// The service that received the operation.
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_moment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_moment: Option<String>,
    /// How many decision-trace entries the slice retains.
    pub decision_trace_entries: usize,
    /// `collected`, `unverified`, or `unavailable`: whether every service's
    /// cumulative serial slice was reconstructed and digest-verified from
    /// the retained run directory.
    pub serial_slices: &'static str,
    /// Every collected file except the manifest itself.
    pub files: Vec<CollectedFile>,
}

/// One located boundary plus its position, so collection can slice the
/// surrounding evidence.
struct LocatedBoundary<'a> {
    run: usize,
    index: usize,
    boundary: &'a serde_json::Value,
    timeline: &'a [serde_json::Value],
    previous: Option<&'a serde_json::Value>,
    next: Option<&'a serde_json::Value>,
}

fn locate_boundary<'a>(
    result: &'a serde_json::Value,
    moment: &str,
) -> Result<LocatedBoundary<'a>, MomentError> {
    let runs = result["runs"]
        .as_array()
        .ok_or_else(|| MomentError::NotFound("result has no runs".to_owned()))?;
    for (run_index, run) in runs.iter().enumerate() {
        let timeline = run["timeline"]
            .as_array()
            .ok_or_else(|| MomentError::NotFound(format!("run {run_index} has no timeline")))?;
        for (boundary_index, boundary) in timeline.iter().enumerate() {
            if boundary["moment"] == *moment {
                let previous = boundary_index
                    .checked_sub(1)
                    .and_then(|index| timeline.get(index));
                let next = timeline.get(boundary_index + 1);
                return Ok(LocatedBoundary {
                    run: run_index,
                    index: boundary_index,
                    boundary,
                    timeline,
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

fn write_collected_file(
    output: &Path,
    relative: &str,
    bytes: &[u8],
    files: &mut Vec<CollectedFile>,
) -> Result<(), MomentError> {
    let path = output.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            MomentError::Read(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("cannot create {}: {source}", parent.display()),
            ))
        })?;
    }
    fs::write(&path, bytes).map_err(|source| {
        MomentError::Read(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("cannot write {}: {source}", path.display()),
        ))
    })?;
    files.push(CollectedFile {
        path: relative.to_owned(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
    });
    Ok(())
}

/// Collect the evidence window around one moment into a self-contained,
/// read-only-auditable artifact directory: the boundary's full record, its
/// neighbors, the decision-trace slice that produced it, and digest-verified
/// cumulative serial-log slices from the retained run directory when it is
/// available. The source bundle is never modified.
pub fn collect_moment(
    bundle: impl AsRef<Path>,
    moment: &str,
    output: impl AsRef<Path>,
) -> Result<CollectedMoment, MomentError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(MomentError::Read)?;
    let path = bundle.join("campaign-result.json");
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let located = locate_boundary(&result, moment)?;
    let boundary = located.boundary;
    let boundary_id = boundary["id"].as_str().unwrap_or_default().to_owned();
    let boundary_service = boundary["service"].as_str().unwrap_or_default().to_owned();
    let boundary_operation = boundary["operation"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let previous_moment = located
        .previous
        .and_then(|boundary| boundary["moment"].as_str())
        .map(str::to_owned);
    let next_moment = located
        .next
        .and_then(|boundary| boundary["moment"].as_str())
        .map(str::to_owned);

    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(MomentError::NotFound(format!(
            "collected output already exists: {}",
            output.display()
        )));
    }
    fs::create_dir_all(&output).map_err(MomentError::Read)?;
    let mut files = Vec::new();

    let encoded =
        |value: &serde_json::Value| serde_json::to_vec_pretty(value).map_err(MomentError::Parse);
    write_collected_file(&output, "boundary.json", &encoded(boundary)?, &mut files)?;
    if let Some(previous) = located.previous {
        write_collected_file(&output, "previous.json", &encoded(previous)?, &mut files)?;
    }
    if let Some(next) = located.next {
        write_collected_file(&output, "next.json", &encoded(next)?, &mut files)?;
    }

    // The decision trace interleaves template headers with
    // `boundary:<position>:` entries; the slice keeps everything up to and
    // including this boundary's decisions.
    let trace = result["runs"][located.run]["decision_trace"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let trace_slice: Vec<&serde_json::Value> = trace
        .iter()
        .filter(|entry| {
            entry
                .as_str()
                .and_then(|entry| entry.strip_prefix("boundary:"))
                .and_then(|rest| rest.split(':').next())
                .and_then(|position| position.parse::<usize>().ok())
                .map(|position| position <= located.index)
                .unwrap_or(true)
        })
        .collect();
    let trace_record = serde_json::json!({
        "format": "theseus-collected-decision-trace-v1",
        "run": located.run,
        "boundary": boundary_id,
        "boundary_index": located.index,
        "entries": trace_slice,
    });
    write_collected_file(
        &output,
        "decision-trace.json",
        &encoded(&trace_record)?,
        &mut files,
    )?;

    // Cumulative serial slices: each boundary's delta bytes accumulate to
    // the transcript length at that moment, verified against the boundary's
    // cumulative digest. Missing or mismatching evidence degrades the
    // collection instead of failing it.
    let run_dir = bundle.join("runs").join(format!("{:03}", located.run));
    let mut serial_slices = "unavailable";
    if run_dir.is_dir() {
        serial_slices = "collected";
        let cumulative_hashes: BTreeMap<String, String> = boundary["serial_sha256"]
            .as_object()
            .map(|hashes| {
                hashes
                    .iter()
                    .map(|(service, hash)| {
                        (
                            service.clone(),
                            hash.as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (service, expected) in &cumulative_hashes {
            let mut length = 0_usize;
            for earlier in located.timeline.iter().take(located.index + 1) {
                length += earlier["serial_delta"][service]["bytes"]
                    .as_u64()
                    .unwrap_or_default() as usize;
            }
            let transcript = run_serial_contents(&run_dir, service);
            let verified = transcript.len() >= length
                && format!("{:x}", Sha256::digest(&transcript[..length])) == *expected;
            if verified {
                write_collected_file(
                    &output,
                    &format!("serial/{service}.log"),
                    &transcript[..length],
                    &mut files,
                )?;
            } else {
                serial_slices = "unverified";
            }
        }
    }

    let collected = CollectedMoment {
        format: "theseus-collected-artifacts-v1",
        source: bundle.display().to_string(),
        run: located.run,
        moment: moment.to_owned(),
        boundary: boundary_id,
        operation: boundary_operation,
        service: boundary_service,
        previous_moment,
        next_moment,
        decision_trace_entries: trace_slice.len(),
        serial_slices,
        files,
    };
    write_collected_file(
        &output,
        "manifest.json",
        &encoded(&serde_json::to_value(&collected).map_err(MomentError::Parse)?)?,
        &mut Vec::new(),
    )?;
    Ok(collected)
}

/// Reconstruct one service's complete serial transcript from a retained run
/// directory, in the same rotation order the runner writes and reads it.
fn run_serial_contents(run: &Path, service: &str) -> Vec<u8> {
    let mut logs = fs::read_dir(run.join("services").join(service))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == "serial.log" || (name.starts_with("serial-") && name.ends_with(".log"))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    logs.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| {
                name.strip_prefix("serial-")
                    .and_then(|suffix| suffix.strip_suffix(".log"))
                    .and_then(|index| index.parse::<usize>().ok())
            })
            .map(|index| index + 1)
            .unwrap_or(0)
    });
    logs.into_iter()
        .flat_map(|path| fs::read(path).unwrap_or_default())
        .collect()
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

    /// Retained excerpts are ASCII-escaped, so the fixture stores literal
    /// backslash-n sequences exactly like a campaign result does.
    fn temporal_fixture() -> serde_json::Value {
        serde_json::json!({
            "runs": [
                {"index": 0, "timeline": [
                    {"id": "op-000-write", "operation": "write", "service": "api",
                     "moment": "7000@input-hash",
                     "serial_delta": {"api": {"bytes": 16, "sha256": "h0", "excerpt": "write\\ncomplete\\n", "omitted_bytes": 0}}},
                    {"id": "op-001-read", "operation": "read", "service": "counter",
                     "moment": "9000@read-hash",
                     "serial_delta": {"counter": {"bytes": 11, "sha256": "h1", "excerpt": "THES:M:stale\\n", "omitted_bytes": 0}}},
                    {"id": "op-002-verify", "operation": "verify", "service": "api",
                     "moment": "12000@verify-hash",
                     "serial_delta": {"api": {"bytes": 8, "sha256": "h2", "excerpt": "done\\n", "omitted_bytes": 0}}}
                ]},
                {"index": 1, "timeline": [
                    {"id": "op-000-write", "operation": "write", "service": "api",
                     "moment": "7000@other-hash",
                     "serial_delta": {"api": {"bytes": 4, "sha256": "h3", "excerpt": "done\\n", "omitted_bytes": 0}}}
                ]}
            ]
        })
    }

    #[test]
    fn temporal_relations_split_moments_around_their_occurrence() {
        let result = temporal_fixture();
        let preceded =
            temporal_query(&result, TemporalRelation::PrecededBy, "stale", None).unwrap();
        assert_eq!(preceded.relation, "preceded_by");
        assert_eq!(preceded.occurrences.len(), 1);
        assert_eq!(preceded.occurrences[0].service, "counter");
        assert_eq!(preceded.occurrences[0].boundary, "op-001-read");
        assert_eq!(preceded.occurrences[0].moment, "9000@read-hash");
        // Strictly before: the stale marker's own boundary and the unrelated
        // second run's timeline satisfy nothing.
        assert_eq!(preceded.matches.len(), 1);
        assert_eq!(preceded.matches[0].boundary, "op-002-verify");
        assert_eq!(preceded.matches[0].moment, "12000@verify-hash");

        let followed =
            temporal_query(&result, TemporalRelation::FollowedBy, "stale", None).unwrap();
        assert_eq!(followed.matches.len(), 1);
        assert_eq!(followed.matches[0].boundary, "op-000-write");
        assert_eq!(followed.matches[0].moment, "7000@input-hash");

        // The needle's own boundary satisfies neither relation.
        for query in [preceded, followed] {
            assert!(query
                .matches
                .iter()
                .all(|summary| summary.boundary != "op-001-read"));
        }
    }

    #[test]
    fn temporal_queries_match_escaped_serial_bytes() {
        let result = temporal_fixture();
        let query = temporal_query(
            &result,
            TemporalRelation::PrecededBy,
            "write\ncomplete",
            None,
        )
        .unwrap();
        assert_eq!(query.needle, "write\\ncomplete");
        assert_eq!(query.occurrences.len(), 1);
        assert_eq!(query.occurrences[0].run, 0);
        assert_eq!(query.occurrences[0].boundary, "op-000-write");
        assert_eq!(query.matches.len(), 2);
        assert_eq!(query.matches[0].boundary, "op-001-read");
        assert_eq!(query.matches[1].boundary, "op-002-verify");
    }

    #[test]
    fn temporal_service_filter_scopes_needle_and_matches() {
        let result = temporal_fixture();
        // Scoped to api, the needle prints at op-000-write; the only api
        // boundary strictly after it is op-002-verify.
        let query = temporal_query(
            &result,
            TemporalRelation::PrecededBy,
            "write\ncomplete",
            Some("api"),
        )
        .unwrap();
        assert_eq!(query.service.as_deref(), Some("api"));
        assert_eq!(query.occurrences.len(), 1);
        assert_eq!(query.occurrences[0].service, "api");
        assert_eq!(query.matches.len(), 1);
        assert_eq!(query.matches[0].boundary, "op-002-verify");

        // Unfiltered, the counter boundary op-001-read also matched; scoped
        // to counter, neither the needle search nor the matches do.
        let query = temporal_query(
            &result,
            TemporalRelation::PrecededBy,
            "write\ncomplete",
            Some("counter"),
        )
        .unwrap();
        assert!(query.occurrences.is_empty());
        assert!(query.matches.is_empty());
    }

    #[test]
    fn temporal_queries_reject_empty_needles() {
        let error = temporal_query(&temporal_fixture(), TemporalRelation::PrecededBy, "", None)
            .unwrap_err();
        assert!(error.to_string().contains("non-empty needle"), "{error}");
    }

    /// A one-run bundle whose second boundary carries a verified cumulative
    /// serial hash and a decision trace with a template header.
    fn collect_fixture(bundle: &Path, transcript: &[u8]) {
        use sha2::{Digest, Sha256};
        let cumulative = format!("{:x}", Sha256::digest(transcript));
        let early = format!("{:x}", Sha256::digest(&transcript[..5]));
        let directory = bundle;
        fs::create_dir_all(directory.join("runs/000/services/counter")).unwrap();
        fs::write(directory.join("campaign-result.json"), format!(r#"{{"runs":[{{"index":0,"decision_trace":["test_template:main","boundary:0:operation:write","boundary:1:operation:read"],"timeline":[
            {{"id":"op-000-write","operation":"write","service":"counter","moment":"7000@input-hash",
             "serial_sha256":{{"counter":"{early}"}},
             "serial_delta":{{"counter":{{"bytes":5,"sha256":"d0","excerpt":"READY","omitted_bytes":0}}}}}},
            {{"id":"op-001-read","operation":"read","service":"counter","moment":"9000@read-hash",
             "serial_sha256":{{"counter":"{cumulative}"}},
             "serial_delta":{{"counter":{{"bytes":{after},"sha256":"d1","excerpt":"log output","omitted_bytes":0}}}}}}
        ]}}]}}"#, after = transcript.len() - 5)).unwrap();
        fs::write(
            directory
                .join("runs/000/services/counter")
                .join("serial.log"),
            transcript,
        )
        .unwrap();
    }

    #[test]
    fn collection_copies_the_boundary_window_and_verifies_serial_slices() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        let transcript = b"READYlog output\n".to_vec();
        collect_fixture(&bundle, &transcript);
        let source_files: std::collections::BTreeSet<String> = fs::read_dir(&bundle)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        let output = directory.path().join("collected");
        let collected = collect_moment(&bundle, "9000@read-hash", &output).unwrap();
        assert_eq!(collected.format, "theseus-collected-artifacts-v1");
        assert_eq!(collected.run, 0);
        assert_eq!(collected.boundary, "op-001-read");
        assert_eq!(
            collected.previous_moment,
            Some("7000@input-hash".to_owned())
        );
        assert_eq!(collected.next_moment, None);
        assert_eq!(collected.serial_slices, "collected");
        assert_eq!(collected.decision_trace_entries, 3);

        // Every manifest entry matches the bytes on disk, and the manifest
        // lists everything except itself.
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["format"], "theseus-collected-artifacts-v1");
        let listed = manifest["files"].as_array().unwrap();
        assert_eq!(listed.len(), collected.files.len());
        for entry in listed {
            let bytes = fs::read(output.join(entry["path"].as_str().unwrap())).unwrap();
            assert_eq!(
                entry["sha256"],
                format!("{:x}", Sha256::digest(&bytes)),
                "{}",
                entry["path"]
            );
        }
        for name in ["boundary.json", "previous.json", "decision-trace.json"] {
            assert!(output.join(name).is_file(), "{name}");
        }
        assert_eq!(collected.files.len(), 4);
        assert!(!output.join("next.json").exists());

        // The serial slice is exactly the cumulative transcript at the
        // boundary, and the source bundle is untouched.
        assert_eq!(
            fs::read(output.join("serial/counter.log")).unwrap(),
            transcript
        );
        assert_eq!(
            source_files,
            fs::read_dir(&bundle)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<std::collections::BTreeSet<String>>()
        );

        // The decision-trace slice keeps the header and both boundaries'
        // entries for the last boundary, and stops earlier for the first.
        let trace: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("decision-trace.json")).unwrap()).unwrap();
        assert_eq!(trace["boundary_index"], 1);
        assert_eq!(trace["entries"].as_array().unwrap().len(), 3);

        let first = directory.path().join("first");
        collect_moment(&bundle, "7000@input-hash", &first).unwrap();
        let trace: serde_json::Value =
            serde_json::from_slice(&fs::read(first.join("decision-trace.json")).unwrap()).unwrap();
        assert_eq!(trace["entries"].as_array().unwrap().len(), 2);
        assert!(!first.join("previous.json").exists());
        assert!(first.join("next.json").exists());
        // Only the first five transcript bytes were visible at that moment.
        assert_eq!(
            fs::read(first.join("serial/counter.log")).unwrap(),
            &transcript[..5]
        );
    }

    #[test]
    fn collection_degrades_without_serial_evidence_and_refuses_existing_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        collect_fixture(&bundle, b"READYlog output\n");
        fs::remove_dir_all(bundle.join("runs")).unwrap();

        let output = directory.path().join("collected");
        let collected = collect_moment(&bundle, "9000@read-hash", &output).unwrap();
        assert_eq!(collected.serial_slices, "unavailable");
        assert!(!output.join("serial").exists());
        assert_eq!(collected.files.len(), 3);

        let error = collect_moment(&bundle, "9000@read-hash", &output).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("collected output already exists"),
            "{error}"
        );

        let error = collect_moment(&bundle, "1@missing", &output).unwrap_err();
        assert!(error.to_string().contains("1@missing"), "{error}");
    }

    #[test]
    fn collection_reports_unverifiable_serial_slices() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("bundle");
        collect_fixture(&bundle, b"READYlog output\n");
        fs::write(
            bundle.join("runs/000/services/counter/serial.log"),
            b"tampered contents",
        )
        .unwrap();
        let output = directory.path().join("collected");
        let collected = collect_moment(&bundle, "9000@read-hash", &output).unwrap();
        assert_eq!(collected.serial_slices, "unverified");
        assert!(!output.join("serial").exists());
    }
}
