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
    /// Guest-emitted JSON event lines on this boundary's serial delta,
    /// verbatim per service.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub events: BTreeMap<String, Vec<String>>,
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
                let mut events = BTreeMap::new();
                if let Some(boundary_events) = boundary["events"].as_object() {
                    for (service, lines) in boundary_events {
                        let lines = lines
                            .as_array()
                            .map(|lines| {
                                lines
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .map(str::to_owned)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        if !lines.is_empty() {
                            events.insert(service.clone(), lines);
                        }
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
                    events,
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

/// One guest-emitted application event, retained verbatim on the boundary
/// where its serial bytes landed.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct EventRecord {
    /// Index of the retained timeline (0-based).
    pub run: usize,
    /// `op-NNN-<operation>` boundary identity.
    pub boundary: String,
    /// The operation name.
    pub operation: String,
    /// The moment address of the boundary carrying the event.
    pub moment: String,
    /// The service whose serial delta contains the event.
    pub service: String,
    /// The event line, verbatim.
    pub line: String,
}

/// List every guest-emitted JSON event across the retained campaigns, in
/// timeline order. Events are the verbatim serial lines; the boundary's
/// moment address retrieves the surrounding evidence.
pub fn list_events(
    result: &serde_json::Value,
    service: Option<&str>,
) -> Result<Vec<EventRecord>, MomentError> {
    let runs = result["runs"]
        .as_array()
        .ok_or_else(|| MomentError::NotFound("result has no runs".to_owned()))?;
    let mut records = Vec::new();
    for (run_index, run) in runs.iter().enumerate() {
        let timeline = run["timeline"]
            .as_array()
            .ok_or_else(|| MomentError::NotFound(format!("run {run_index} has no timeline")))?;
        for boundary in timeline {
            if let Some(events) = boundary["events"].as_object() {
                for (event_service, lines) in events {
                    if let Some(filter) = service {
                        if event_service != filter {
                            continue;
                        }
                    }
                    for line in lines
                        .as_array()
                        .map(|lines| lines.as_slice())
                        .unwrap_or(&[])
                    {
                        if let Some(line) = line.as_str() {
                            records.push(EventRecord {
                                run: run_index,
                                boundary: boundary["id"].as_str().unwrap_or_default().to_owned(),
                                operation: boundary["operation"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_owned(),
                                service: event_service.clone(),
                                moment: boundary["moment"].as_str().unwrap_or_default().to_owned(),
                                line: line.to_owned(),
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(records)
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
    /// The optional window bound the relation was evaluated with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within: Option<usize>,
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

/// The largest window a temporal query accepts; windows count boundaries
/// between the anchor and the match.
const QUERY_WINDOW_LIMIT: usize = 4096;

/// Validate the optional window bound: it counts boundaries between the
/// anchor and the match, so it must be at least 1.
fn validate_within(within: Option<usize>) -> Result<(), MomentError> {
    match within {
        None => Ok(()),
        Some(0) => Err(MomentError::NotFound(
            "temporal query windows count boundaries and must be at least 1".to_owned(),
        )),
        Some(window) if window > QUERY_WINDOW_LIMIT => Err(MomentError::NotFound(format!(
            "temporal query windows are bounded at {QUERY_WINDOW_LIMIT} boundaries"
        ))),
        Some(_) => Ok(()),
    }
}

/// True when some occurrence relates to the boundary under the relation,
/// inside the optional window: `within` of 1 means the immediately
/// adjacent boundary, and no window means the whole run.
fn related_within(
    relation: TemporalRelation,
    occurrence_indices: &[usize],
    boundary_index: usize,
    within: Option<usize>,
) -> bool {
    occurrence_indices.iter().any(|occurrence| match relation {
        TemporalRelation::PrecededBy => {
            *occurrence < boundary_index
                && within.is_none_or(|window| boundary_index - *occurrence <= window)
        }
        TemporalRelation::FollowedBy => {
            *occurrence > boundary_index
                && within.is_none_or(|window| *occurrence - boundary_index <= window)
        }
    })
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
    within: Option<usize>,
) -> Result<TemporalQuery, MomentError> {
    validate_within(within)?;

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
            let related = related_within(relation, &occurrence_indices, boundary_index, within);
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
        within,
        relation: relation.as_str(),
        needle: escaped_needle,
        service: service.map(str::to_owned),
        occurrences,
        matches,
    })
}

/// The answer to one event-object temporal query: where matching events
/// printed, and every moment satisfying the relation. The event lines are
/// JSON objects, so the predicate maps RFC 6901 pointers to expected
/// values - the same `fields` shape the property layer's JSON predicates
/// accept - and every pointer must resolve equal.
#[derive(Debug, Serialize)]
pub struct EventTemporalQuery {
    /// The optional window bound the relation was evaluated with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within: Option<usize>,
    pub format: &'static str,
    pub relation: &'static str,
    /// The predicate as parsed from the command line.
    pub predicate: serde_json::Value,
    /// The service scope filter, echoed back when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Every matching event, verbatim.
    pub occurrences: Vec<EventRecord>,
    /// Every moment satisfying the relation against those occurrences.
    pub matches: Vec<MomentSummary>,
}

/// True when every pointer in the fields map resolves on the event to an
/// equal value.
fn event_matches_fields(
    event: &serde_json::Value,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    fields.iter().all(|(pointer, expected)| {
        event
            .pointer(pointer)
            .is_some_and(|actual| actual == expected)
    })
}

/// The nesting cap shared by validation and evaluation; deeper predicates
/// are refused rather than recursed.
const PREDICATE_DEPTH_LIMIT: usize = 8;

/// Validate one query predicate against the shared grammar: the same
/// fields/where/arrays/all/any/none shape the property layer evaluates on
/// the guest side, so one grammar answers everywhere. Capture keys bind
/// across ordered serial items, which a one-event query cannot mean, so
/// they are refused by name.
fn validate_query_predicate(predicate: &serde_json::Value) -> Result<(), MomentError> {
    validate_query_predicate_at_depth(predicate, 0)
}

fn validate_query_predicate_at_depth(
    predicate: &serde_json::Value,
    depth: usize,
) -> Result<(), MomentError> {
    if depth > PREDICATE_DEPTH_LIMIT {
        return Err(MomentError::NotFound(format!(
            "event predicate nests deeper than {PREDICATE_DEPTH_LIMIT} levels"
        )));
    }
    let Some(object) = predicate.as_object() else {
        return Err(MomentError::NotFound(
            "event predicate must be a JSON object with a fields map of pointer-to-value entries"
                .to_owned(),
        ));
    };
    let recognized = ["query", "fields", "where", "arrays", "all", "any", "none"];
    if !object.keys().any(|key| recognized.contains(&key.as_str())) {
        return Err(MomentError::NotFound(
            "event predicate must set at least one of query, fields, where, arrays, all, any, none"
                .to_owned(),
        ));
    }
    for key in object.keys() {
        if key == "capture" || key == "equals_capture" {
            return Err(MomentError::NotFound(
                "event predicate cannot use capture or equals_capture: they bind values across ordered serial items, which a one-event query cannot mean".to_owned(),
            ));
        }
        if !recognized.contains(&key.as_str()) {
            return Err(MomentError::NotFound(format!(
                "event predicate has an unknown key: {key:?}"
            )));
        }
    }
    if let Some(query) = object.get("query") {
        let Some(query) = query.as_str() else {
            return Err(MomentError::NotFound(
                "event predicate query must be an RFC 9535 JSONPath string".to_owned(),
            ));
        };
        if serde_json_path::JsonPath::parse(query).is_err() {
            return Err(MomentError::NotFound(
                "event predicate query is not a valid RFC 9535 JSONPath".to_owned(),
            ));
        }
    }
    if object.contains_key("fields") && !object["fields"].is_object() {
        return Err(MomentError::NotFound(
            "event predicate fields must be an object of pointer-to-value entries".to_owned(),
        ));
    }
    for key in ["where", "arrays", "all", "any", "none"] {
        if object.contains_key(key) && !object[key].is_array() {
            return Err(MomentError::NotFound(format!(
                "event predicate {key} must be an array"
            )));
        }
    }
    if let Some(fields) = object.get("fields").and_then(serde_json::Value::as_object) {
        if fields.is_empty() {
            return Err(MomentError::NotFound(
                "event predicate needs at least one pointer field".to_owned(),
            ));
        }
        for pointer in fields.keys() {
            if !pointer.starts_with('/') {
                return Err(MomentError::NotFound(format!(
                    "event predicate fields must use RFC 6901 pointers starting with '/': {pointer:?}"
                )));
            }
        }
    }
    if let Some(conditions) = object.get("where").and_then(serde_json::Value::as_array) {
        for condition in conditions {
            let Some(condition) = condition.as_object() else {
                return Err(MomentError::NotFound(
                    "event predicate where entries must be JSON objects".to_owned(),
                ));
            };
            let Some(pointer) = condition.get("pointer").and_then(serde_json::Value::as_str) else {
                return Err(MomentError::NotFound(
                    "event predicate where entries need a pointer".to_owned(),
                ));
            };
            if !pointer.starts_with('/') {
                return Err(MomentError::NotFound(format!(
                    "event predicate where pointers must use RFC 6901 pointers starting with '/': {pointer:?}"
                )));
            }
            let operators = [
                "equals",
                "matches",
                "greater_than",
                "greater_than_or_equal",
                "less_than",
                "less_than_or_equal",
                "exists",
            ];
            if !condition
                .keys()
                .any(|key| key == "pointer" || operators.contains(&key.as_str()))
            {
                return Err(MomentError::NotFound(
                    "event predicate where entries need one of equals, matches, greater_than, greater_than_or_equal, less_than, less_than_or_equal, exists".to_owned(),
                ));
            }
            for key in condition.keys() {
                if key != "pointer" && !operators.contains(&key.as_str()) {
                    return Err(MomentError::NotFound(format!(
                        "event predicate where entry has an unknown key: {key:?}"
                    )));
                }
            }
        }
    }
    if let Some(arrays) = object.get("arrays").and_then(serde_json::Value::as_array) {
        for array in arrays {
            let Some(array) = array.as_object() else {
                return Err(MomentError::NotFound(
                    "event predicate arrays entries must be JSON objects".to_owned(),
                ));
            };
            let Some(pointer) = array.get("pointer").and_then(serde_json::Value::as_str) else {
                return Err(MomentError::NotFound(
                    "event predicate arrays entries need a pointer".to_owned(),
                ));
            };
            if !pointer.starts_with('/') {
                return Err(MomentError::NotFound(format!(
                    "event predicate arrays pointers must use RFC 6901 pointers starting with '/': {pointer:?}"
                )));
            }
            for key in ["any", "all", "none"] {
                if let Some(nested) = array.get(key) {
                    validate_query_predicate_at_depth(nested, depth + 1)?;
                }
            }
        }
    }
    for key in ["all", "any", "none"] {
        if let Some(nested) = object.get(key).and_then(serde_json::Value::as_array) {
            for predicate in nested {
                validate_query_predicate_at_depth(predicate, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// True when one where-condition holds on the event.
fn json_condition_matches(event: &serde_json::Value, condition: &serde_json::Value) -> bool {
    let Some(pointer) = condition.get("pointer").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let actual = event.pointer(pointer);
    if let Some(expected) = condition.get("exists").and_then(serde_json::Value::as_bool) {
        return actual.is_some() == expected;
    }
    if let Some(expected) = condition.get("equals") {
        return actual == Some(expected);
    }
    if let Some(expression) = condition.get("matches").and_then(serde_json::Value::as_str) {
        return actual
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| {
                regex::Regex::new(expression)
                    .map(|expression| expression.is_match(value))
                    .unwrap_or(false)
            });
    }
    let Some(actual) = actual.and_then(serde_json::Value::as_f64) else {
        return false;
    };
    for (key, compare) in [
        ("greater_than", std::cmp::Ordering::Greater),
        ("greater_than_or_equal", std::cmp::Ordering::Greater),
        ("less_than", std::cmp::Ordering::Less),
        ("less_than_or_equal", std::cmp::Ordering::Less),
    ] {
        if let Some(expected) = condition.get(key).and_then(serde_json::Value::as_f64) {
            let ordering = actual
                .partial_cmp(&expected)
                .unwrap_or(std::cmp::Ordering::Equal);
            let satisfied = match key {
                "greater_than" | "less_than" => ordering == compare,
                _ => ordering == compare || ordering == std::cmp::Ordering::Equal,
            };
            return satisfied;
        }
    }
    false
}

/// True when one nested quantifier (any, all, or none) holds over the
/// array's elements for the nested predicate.
fn array_quantifier_matches(
    values: &[serde_json::Value],
    nested: &serde_json::Value,
    key: &str,
) -> bool {
    match key {
        "any" => values.iter().any(|value| predicate_matches(value, nested)),
        "all" => values.iter().all(|value| predicate_matches(value, nested)),
        _ => values.iter().all(|value| !predicate_matches(value, nested)),
    }
}

/// True when the event satisfies the shared predicate grammar - the same
/// fields/where/arrays/all/any/none shape the property layer evaluates on
/// the guest side, in the same order.
fn predicate_matches(event: &serde_json::Value, predicate: &serde_json::Value) -> bool {
    if let Some(query) = predicate.get("query").and_then(serde_json::Value::as_str) {
        let selected = serde_json_path::JsonPath::parse(query)
            .map(|query| !query.query(event).all().is_empty())
            .unwrap_or(false);
        if !selected {
            return false;
        }
    }
    if let Some(fields) = predicate
        .get("fields")
        .and_then(serde_json::Value::as_object)
    {
        if !event_matches_fields(event, fields) {
            return false;
        }
    }
    if let Some(conditions) = predicate.get("where").and_then(serde_json::Value::as_array) {
        if !conditions
            .iter()
            .all(|condition| json_condition_matches(event, condition))
        {
            return false;
        }
    }
    if let Some(arrays) = predicate
        .get("arrays")
        .and_then(serde_json::Value::as_array)
    {
        for array in arrays {
            let Some(pointer) = array.get("pointer").and_then(serde_json::Value::as_str) else {
                return false;
            };
            let Some(values) = event.pointer(pointer).and_then(serde_json::Value::as_array) else {
                return false;
            };
            for key in ["any", "all", "none"] {
                if let Some(nested) = array.get(key) {
                    if !array_quantifier_matches(values, nested, key) {
                        return false;
                    }
                }
            }
        }
    }
    for key in ["all", "any", "none"] {
        if let Some(nested) = predicate.get(key).and_then(serde_json::Value::as_array) {
            let hits = nested
                .iter()
                .filter(|inner| predicate_matches(event, inner))
                .count();
            let satisfied = match key {
                "all" => hits == nested.len(),
                "any" => hits > 0,
                _ => hits == 0,
            };
            if !satisfied {
                return false;
            }
        }
    }
    true
}

/// Evaluate a `preceded-by`/`followed-by` relation between one structured
/// event predicate and every retained moment. The predicate is a JSON
/// object mapping RFC 6901 pointers to expected values; an indexed event
/// matches when every pointer resolves to an equal value, and a moment
/// matches `preceded_by` when some matching event lies strictly before it
/// in the same run (`followed_by`: strictly after).
pub fn event_temporal_query(
    result: &serde_json::Value,
    relation: TemporalRelation,
    predicate: &serde_json::Value,
    service: Option<&str>,
    within: Option<usize>,
) -> Result<EventTemporalQuery, MomentError> {
    validate_within(within)?;

    // The predicate uses the property layer's `fields` shape: RFC 6901
    // pointers to expected values, all of which must match.
    validate_query_predicate(predicate)?;
    let runs = result["runs"]
        .as_array()
        .ok_or_else(|| MomentError::NotFound("result has no runs".to_owned()))?;
    let mut occurrences = Vec::new();
    let mut matches = Vec::new();
    for (run_index, run) in runs.iter().enumerate() {
        let timeline = run["timeline"]
            .as_array()
            .ok_or_else(|| MomentError::NotFound(format!("run {run_index} has no timeline")))?;
        let mut occurrence_indices = Vec::new();
        for (boundary_index, boundary) in timeline.iter().enumerate() {
            let mut boundary_matches = false;
            if let Some(events) = boundary["events"].as_object() {
                for (event_service, lines) in events {
                    if let Some(filter) = service {
                        if event_service != filter {
                            continue;
                        }
                    }
                    for line in lines
                        .as_array()
                        .map(|lines| lines.as_slice())
                        .unwrap_or(&[])
                    {
                        let Some(line) = line.as_str() else {
                            continue;
                        };
                        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                            continue;
                        };

                        if predicate_matches(&event, predicate) {
                            boundary_matches = true;
                            occurrences.push(EventRecord {
                                run: run_index,
                                boundary: boundary["id"].as_str().unwrap_or_default().to_owned(),
                                operation: boundary["operation"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_owned(),
                                service: event_service.clone(),
                                moment: boundary["moment"].as_str().unwrap_or_default().to_owned(),
                                line: line.to_owned(),
                            });
                        }
                    }
                }
            }
            if boundary_matches {
                occurrence_indices.push(boundary_index);
            }
        }
        for (boundary_index, boundary) in timeline.iter().enumerate() {
            let related = related_within(relation, &occurrence_indices, boundary_index, within);
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
    Ok(EventTemporalQuery {
        format: "theseus-query-event-temporal-v1",
        within,
        relation: relation.as_str(),
        predicate: predicate.clone(),
        service: service.map(str::to_owned),
        occurrences,
        matches,
    })
}

/// The answer to one composed query: every moment whose own boundary
/// carried a guest event matching the predicate, related to a needle
/// occurrence strictly before (or after) it in the same run. This composes
/// the property layer's RFC 6901 `fields` shape with the needle relations,
/// so a moment qualifies only when its neighboring events carry the
/// expected values.
#[derive(Debug, Serialize)]
pub struct PredicateQuery {
    /// The optional window bound the relation was evaluated with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within: Option<usize>,
    pub format: &'static str,
    pub relation: &'static str,
    pub needle: String,
    /// The event predicate the matches' boundaries must satisfy.
    pub predicate: serde_json::Value,
    /// The service scope filter, echoed back when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Every needle occurrence, verbatim.
    pub occurrences: Vec<NeedleOccurrence>,
    /// Every moment satisfying both the relation and the predicate.
    pub matches: Vec<MomentSummary>,
}

/// True when the boundary's indexed guest events contain at least one line
/// matching the predicate.
fn boundary_carries_matching_event(
    boundary: &serde_json::Value,
    predicate: &serde_json::Value,
    service: Option<&str>,
) -> bool {
    let Some(events) = boundary["events"].as_object() else {
        return false;
    };
    events.iter().any(|(event_service, lines)| {
        if let Some(filter) = service {
            if event_service != filter {
                return false;
            }
        }
        lines
            .as_array()
            .map(|lines| lines.as_slice())
            .unwrap_or(&[])
            .iter()
            .any(|line| {
                line.as_str()
                    .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .is_some_and(|event| predicate_matches(&event, predicate))
            })
    })
}

/// Evaluate one composed needle-plus-event-predicate query. The needle
/// locates occurrences in serial deltas exactly like the plain temporal
/// query; a match additionally requires its own boundary to carry a guest
/// event matching every field pointer.
pub fn predicate_query(
    result: &serde_json::Value,
    relation: TemporalRelation,
    needle: &str,
    predicate: &serde_json::Value,
    service: Option<&str>,
    within: Option<usize>,
) -> Result<PredicateQuery, MomentError> {
    validate_within(within)?;

    if needle.is_empty() {
        return Err(MomentError::NotFound(
            "temporal query requires a non-empty needle".to_owned(),
        ));
    }
    validate_query_predicate(predicate)?;
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
            let related = related_within(relation, &occurrence_indices, boundary_index, within);
            if !related {
                continue;
            }
            let boundary_service = boundary["service"].as_str().unwrap_or_default().to_owned();
            if let Some(filter) = service {
                if boundary_service != filter {
                    continue;
                }
            }
            if !boundary_carries_matching_event(boundary, predicate, service) {
                continue;
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
    Ok(PredicateQuery {
        format: "theseus-query-predicate-v1",
        within,
        relation: relation.as_str(),
        needle: escaped_needle,
        predicate: predicate.clone(),
        service: service.map(str::to_owned),
        occurrences,
        matches,
    })
}

/// The answer to one two-predicate query: every moment whose own boundary
/// carried a guest event matching the where-predicate, related to a
/// relation-predicate event strictly before (or after) it in the same run.
/// This composes the event-field relations with a second predicate, so
/// "moments that emitted the retry request after the stale marker
/// printed" is one query.
#[derive(Debug, Serialize)]
pub struct EventPredicateQuery {
    /// The optional window bound the relation was evaluated with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within: Option<usize>,
    pub format: &'static str,
    pub relation: &'static str,
    /// The relation predicate locating the anchor events.
    pub relation_predicate: serde_json::Value,
    /// The where-predicate the matches' boundaries must satisfy.
    pub where_predicate: serde_json::Value,
    /// The service scope filter, echoed back when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Every matching anchor event, verbatim.
    pub occurrences: Vec<EventRecord>,
    /// Every moment satisfying both predicates.
    pub matches: Vec<MomentSummary>,
}

/// Evaluate one two-predicate query: the relation predicate locates anchor
/// events exactly like `event_temporal_query`; a match additionally
/// requires its own boundary to carry a guest event matching the
/// where-predicate.
pub fn event_predicate_query(
    result: &serde_json::Value,
    relation: TemporalRelation,
    relation_predicate: &serde_json::Value,
    where_predicate: &serde_json::Value,
    service: Option<&str>,
    within: Option<usize>,
) -> Result<EventPredicateQuery, MomentError> {
    validate_within(within)?;

    validate_query_predicate(relation_predicate)?;
    validate_query_predicate(where_predicate)?;
    let runs = result["runs"]
        .as_array()
        .ok_or_else(|| MomentError::NotFound("result has no runs".to_owned()))?;
    let mut occurrences = Vec::new();
    let mut matches = Vec::new();
    for (run_index, run) in runs.iter().enumerate() {
        let timeline = run["timeline"]
            .as_array()
            .ok_or_else(|| MomentError::NotFound(format!("run {run_index} has no timeline")))?;
        let mut occurrence_indices = Vec::new();
        for (boundary_index, boundary) in timeline.iter().enumerate() {
            let mut boundary_matches = false;
            if let Some(events) = boundary["events"].as_object() {
                for (event_service, lines) in events {
                    if let Some(filter) = service {
                        if event_service != filter {
                            continue;
                        }
                    }
                    for line in lines
                        .as_array()
                        .map(|lines| lines.as_slice())
                        .unwrap_or(&[])
                    {
                        let Some(line) = line.as_str() else {
                            continue;
                        };
                        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                            continue;
                        };
                        if predicate_matches(&event, relation_predicate) {
                            boundary_matches = true;
                            occurrences.push(EventRecord {
                                run: run_index,
                                boundary: boundary["id"].as_str().unwrap_or_default().to_owned(),
                                operation: boundary["operation"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_owned(),
                                service: event_service.clone(),
                                moment: boundary["moment"].as_str().unwrap_or_default().to_owned(),
                                line: line.to_owned(),
                            });
                        }
                    }
                }
            }
            if boundary_matches {
                occurrence_indices.push(boundary_index);
            }
        }
        for (boundary_index, boundary) in timeline.iter().enumerate() {
            let related = related_within(relation, &occurrence_indices, boundary_index, within);
            if !related {
                continue;
            }
            let boundary_service = boundary["service"].as_str().unwrap_or_default().to_owned();
            if let Some(filter) = service {
                if boundary_service != filter {
                    continue;
                }
            }
            if !boundary_carries_matching_event(boundary, where_predicate, service) {
                continue;
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
    Ok(EventPredicateQuery {
        format: "theseus-query-event-predicate-v1",
        within,
        relation: relation.as_str(),
        relation_predicate: relation_predicate.clone(),
        where_predicate: where_predicate.clone(),
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
    within: Option<usize>,
) -> Result<TemporalQuery, MomentError> {
    let path = bundle.as_ref().join("campaign-result.json");
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    temporal_query(&result, relation, needle, service, within)
}

/// One in-memory collected file awaiting digest and manifest: the path
/// relative to the collection root and its bytes.
type CollectedFileBytes = (String, Vec<u8>);

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
            MomentError::Read(std::io::Error::other(format!(
                "cannot create {}: {source}",
                parent.display()
            )))
        })?;
    }
    fs::write(&path, bytes).map_err(|source| {
        MomentError::Read(std::io::Error::other(format!(
            "cannot write {}: {source}",
            path.display()
        )))
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
    // Locate and build first, so a missing moment outranks an existing
    // output directory in the error a reader sees.
    let (collected, files) = collect_moment_files(bundle, moment)?;
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(MomentError::NotFound(format!(
            "collected output already exists: {}",
            output.display()
        )));
    }
    fs::create_dir_all(&output).map_err(MomentError::Read)?;
    for (relative, bytes) in &files {
        write_collected_file(&output, relative, bytes, &mut Vec::new())?;
    }
    Ok(collected)
}

/// Collect one moment's evidence as in-memory files: the same set
/// [`collect_moment`] writes to disk, so the serve surface can answer a
/// collection without creating directories.
pub fn collect_moment_files(
    bundle: impl AsRef<Path>,
    moment: &str,
) -> Result<(CollectedMoment, Vec<CollectedFileBytes>), MomentError> {
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

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut record_files: Vec<CollectedFile> = Vec::new();
    let mut write = |relative: &str, bytes: &[u8]| -> Result<(), MomentError> {
        files.push((relative.to_owned(), bytes.to_vec()));
        record_files.push(CollectedFile {
            path: relative.to_owned(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        });
        Ok(())
    };
    let encoded =
        |value: &serde_json::Value| serde_json::to_vec_pretty(value).map_err(MomentError::Parse);
    write("boundary.json", &encoded(boundary)?)?;
    if let Some(previous) = located.previous {
        write("previous.json", &encoded(previous)?)?;
    }
    if let Some(next) = located.next {
        write("next.json", &encoded(next)?)?;
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
    write("decision-trace.json", &encoded(&trace_record)?)?;

    // Cumulative serial slices: each boundary's delta bytes accumulate to
    // the transcript length at that moment, verified against the boundary's
    // cumulative digest. Missing or mismatching evidence degrades the
    // collection instead of failing it.
    // The collected run's choice records: the consumed structured-choice
    // values and their feedback, beside the boundary window.
    let run_choices = &result["runs"][located.run];
    let choice_record = serde_json::json!({
        "format": "theseus-collected-choices-v1",
        "run": located.run,
        "structured_choices": run_choices["structured_choices"],
        "choice_feedback": run_choices["choice_feedback"],
    });
    write("choices.json", &encoded(&choice_record)?)?;

    // The progress journal's prefix for this run: the progress line, run
    // record, and checkpoint-ledger lines up to and including the collected
    // run, so the evidence bundle carries the live account beside the
    // boundary window.
    if let Ok(journal) = fs::read_to_string(bundle.join("progress.jsonl")) {
        let mut last_index: i64 = -1;
        let mut prefix = String::new();
        for line in journal.lines() {
            if let Ok(record) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(index) = record["index"].as_i64() {
                    last_index = index;
                }
            }
            if last_index <= located.run as i64 {
                prefix.push_str(line);
                prefix.push('\n');
            }
        }
        if !prefix.is_empty() {
            write("progress.jsonl", prefix.as_bytes())?;
        }
    }
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
                write(&format!("serial/{service}.log"), &transcript[..length])?;
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
        files: record_files,
    };
    files.push((
        "manifest.json".to_owned(),
        encoded(&serde_json::to_value(&collected).map_err(MomentError::Parse)?)?,
    ));
    Ok((collected, files))
}

/// Reconstruct one service's complete serial transcript from a retained run
/// directory, in the same rotation order the runner writes and reads it.
pub(crate) fn run_serial_contents(run: &Path, service: &str) -> Vec<u8> {
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
    fn moments_carry_guest_events_and_events_list_verbatim() {
        let result = serde_json::json!({
            "runs": [{"index": 0, "timeline": [
                {"id": "op-000-write", "operation": "write", "service": "api",
                 "moment": "7000@input-hash",
                 "events": {"api": [
                     "{\"event\":\"request\",\"seq\":1,\"worker\":\"a\"}",
                     "{\"event\":\"request\",\"seq\":2,\"worker\":\"b\"}"
                 ]}},
                {"id": "op-001-read", "operation": "read", "service": "counter",
                 "moment": "9000@read-hash"}
            ]}]
        });

        let hit = find_moment(&result, "7000@input-hash").unwrap();
        check_events(&hit.events, 2);

        let records = list_events(&result, None).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].service, "api");
        assert_eq!(
            records[0].line,
            "{\"event\":\"request\",\"seq\":1,\"worker\":\"a\"}"
        );
        assert_eq!(records[0].moment, "7000@input-hash");
        assert_eq!(records[0].boundary, "op-000-write");

        let filtered = list_events(&result, Some("counter")).unwrap();
        assert!(filtered.is_empty());

        // A boundary without events keeps the hit shape minimal.
        let later = find_moment(&result, "9000@read-hash").unwrap();
        check_events(&later.events, 0);
    }

    fn check_events(events: &BTreeMap<String, Vec<String>>, want: usize) {
        let total: usize = events.values().map(Vec::len).sum();
        assert_eq!(total, want, "{events:?}");
    }

    #[test]
    fn temporal_relations_split_moments_around_their_occurrence() {
        let result = temporal_fixture();
        let preceded =
            temporal_query(&result, TemporalRelation::PrecededBy, "stale", None, None).unwrap();
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
            temporal_query(&result, TemporalRelation::FollowedBy, "stale", None, None).unwrap();
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
            None,
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
            None,
        )
        .unwrap();
        assert!(query.occurrences.is_empty());
        assert!(query.matches.is_empty());
    }

    #[test]
    fn event_predicates_match_json_structured_occurrences() {
        let result = serde_json::json!({
            "runs": [{"index": 0, "timeline": [
                {"id": "op-000-write", "operation": "write", "service": "api",
                 "moment": "7000@input-hash",
                 "events": {"api": [
                     "{\"event\":\"request\",\"seq\":1,\"worker\":\"a\"}",
                     "{\"event\":\"verify\",\"seq\":2}"
                 ]}},
                {"id": "op-001-read", "operation": "read", "service": "counter",
                 "moment": "9000@read-hash",
                 "events": {"counter": ["{\"event\":\"request\",\"seq\":2}"]}}
            ]}]
        });

        // The request event prints on the read boundary's delta; the write
        // boundary is strictly before it and satisfies followed-by, while
        // the occurrence's own boundary satisfies neither relation.
        let followed = event_temporal_query(
            &result,
            TemporalRelation::FollowedBy,
            &serde_json::json!({"fields": {"/event": "request", "/worker": "a"}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(followed.format, "theseus-query-event-temporal-v1");
        assert_eq!(followed.occurrences.len(), 1);
        assert_eq!(
            followed.occurrences[0].line,
            "{\"event\":\"request\",\"seq\":1,\"worker\":\"a\"}"
        );
        assert_eq!(followed.matches.len(), 0);

        let preceded = event_temporal_query(
            &result,
            TemporalRelation::PrecededBy,
            &serde_json::json!({"fields": {"/event": "request", "/worker": "a"}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(preceded.matches.len(), 1);
        assert_eq!(preceded.matches[0].boundary, "op-001-read");
    }

    #[test]
    fn event_predicates_span_services_and_match_nested_pointers() {
        let result = serde_json::json!({
            "runs": [{"index": 0, "timeline": [
                {"id": "op-000-write", "operation": "write", "service": "api",
                 "moment": "7000@input-hash",
                 "events": {"api": [
                     "{\"event\":\"write\",\"seq\":1,\"output\":{\"value\":1}}"
                 ]}},
                {"id": "op-001-read", "operation": "read", "service": "counter",
                 "moment": "9000@read-hash",
                 "events": {"counter": ["{\"event\":\"read\",\"value\":1}"]}}
            ]}]
        });

        let query = event_temporal_query(
            &result,
            TemporalRelation::PrecededBy,
            &serde_json::json!({"fields": {"/output/value": 1}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(query.occurrences.len(), 1);
        assert_eq!(query.occurrences[0].service, "api");
        assert_eq!(query.matches.len(), 1);
        assert_eq!(query.matches[0].boundary, "op-001-read");

        let scoped = event_temporal_query(
            &result,
            TemporalRelation::PrecededBy,
            &serde_json::json!({"fields": {"/output/value": 1}}),
            Some("counter"),
            None,
        )
        .unwrap();
        assert!(scoped.occurrences.is_empty());
        assert!(scoped.matches.is_empty());
    }

    #[test]
    fn event_predicates_reject_malformed_input() {
        let result = temporal_fixture();
        // Not an object, an empty object, and a non-pointer field are errors.
        for predicate in [
            serde_json::json!("request"),
            serde_json::json!({}),
            serde_json::json!({"fields": {}}),
            serde_json::json!({"event": "request"}),
        ] {
            let error = event_temporal_query(
                &result,
                TemporalRelation::PrecededBy,
                &predicate,
                None,
                None,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("event predicate")
                    || error.to_string().contains("non-empty needle"),
                "{error}"
            );
        }
    }

    #[test]
    fn window_bounds_narrow_temporal_relations() {
        // A three-boundary timeline: ready on op-000, stale on op-001,
        // done on op-002.
        let result = temporal_fixture();

        // Without a window, every later boundary relates to the stale
        // occurrence; with within 1, only the immediately adjacent one.
        let unbounded =
            temporal_query(&result, TemporalRelation::PrecededBy, "stale", None, None).unwrap();
        assert_eq!(unbounded.matches.len(), 1);
        assert_eq!(unbounded.matches[0].boundary, "op-002-verify");

        let windowed = temporal_query(
            &result,
            TemporalRelation::PrecededBy,
            "stale",
            None,
            Some(1),
        )
        .unwrap();
        assert_eq!(windowed.within, Some(1));
        assert_eq!(windowed.matches.len(), 1);
        assert_eq!(windowed.matches[0].boundary, "op-002-verify");

        // Followed-by within 1 reaches only the write boundary.
        let followed = temporal_query(
            &result,
            TemporalRelation::FollowedBy,
            "stale",
            None,
            Some(1),
        )
        .unwrap();
        assert_eq!(followed.matches.len(), 1);
        assert_eq!(followed.matches[0].boundary, "op-000-write");

        // A window that excludes every occurrence empties the answer.
        let far = temporal_query(
            &result,
            TemporalRelation::PrecededBy,
            "ready",
            None,
            Some(1),
        )
        .unwrap();
        // ready prints on op-000; the verify boundary is two boundaries
        // later, so within 1 excludes it.
        assert!(far.matches.is_empty(), "{far:?}");

        // Window bounds apply to the composed forms too.
        let composed = predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            "stale",
            &serde_json::json!({"fields": {"/event": "request"}}),
            None,
            Some(1),
        )
        .unwrap();
        assert_eq!(composed.within, Some(1));
        let event_composed = event_predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            &serde_json::json!({"fields": {"/event": "read"}}),
            &serde_json::json!({"fields": {"/event": "write"}}),
            None,
            Some(1),
        )
        .unwrap();
        assert_eq!(event_composed.within, Some(1));

        // Window 0 and oversized windows are named errors.
        for within in [Some(0), Some(4097)] {
            let error =
                temporal_query(&result, TemporalRelation::PrecededBy, "stale", None, within)
                    .unwrap_err();
            assert!(error.to_string().contains("window"), "{error}");
        }
    }

    #[test]
    fn temporal_queries_reject_empty_needles() {
        let error = temporal_query(
            &temporal_fixture(),
            TemporalRelation::PrecededBy,
            "",
            None,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-empty needle"), "{error}");
    }

    /// A bundle whose write boundary carries a request event, the verify
    /// boundary carries a verify event, and the counter's stale marker
    /// anchors the needle relation in the middle of the timeline.
    fn predicate_fixture() -> serde_json::Value {
        serde_json::json!({
            "runs": [{"index": 0, "timeline": [
                {"id": "op-000-write", "operation": "write", "service": "api",
                 "moment": "7000@input-hash",
                 "events": {"api": [
                     "{\"event\":\"request\",\"seq\":1,\"worker\":\"a\"}",
                     "{\"event\":\"request\",\"seq\":2,\"worker\":\"b\"}"
                 ]}},
                {"id": "op-001-read", "operation": "read", "service": "counter",
                 "moment": "9000@read-hash",
                 "serial_delta": {"counter": {"bytes": 11, "sha256": "h1",
                                              "excerpt": "THES:M:stale\\n",
                                              "omitted_bytes": 0}}},
                {"id": "op-002-verify", "operation": "verify", "service": "api",
                 "moment": "12000@verify-hash",
                 "events": {"api": ["{\"event\":\"verify\",\"seq\":3}"]}},
                {"id": "op-003-final", "operation": "final", "service": "api",
                 "moment": "15000@final-hash",
                 "events": {"api": ["{\"event\":\"request\",\"seq\":4,\"worker\":\"a\"}"]}}
            ]}]
        })
    }

    #[test]
    fn predicate_queries_compose_needle_relations_with_event_fields() {
        let result = predicate_fixture();
        let query = predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            "stale",
            &serde_json::json!({"fields": {"/event": "request", "/worker": "a"}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(query.format, "theseus-query-predicate-v1");
        assert_eq!(query.relation, "preceded_by");
        assert_eq!(query.needle, "stale");
        assert_eq!(query.occurrences.len(), 1);
        assert_eq!(query.occurrences[0].boundary, "op-001-read");
        // Strictly after the stale marker: the verify boundary carries a
        // verify event (not a request), the final boundary carries a
        // request from worker a - only the final boundary survives both
        // filters. The write boundary is before the needle, and the
        // needle's own boundary is related to nothing.
        assert_eq!(query.matches.len(), 1);
        assert_eq!(query.matches[0].boundary, "op-003-final");
        assert_eq!(query.matches[0].service, "api");

        let followed = predicate_query(
            &result,
            TemporalRelation::FollowedBy,
            "stale",
            &serde_json::json!({"fields": {"/event": "request", "/worker": "a"}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(followed.matches.len(), 1);
        assert_eq!(followed.matches[0].boundary, "op-000-write");

        // A predicate no boundary satisfies empties the matches but keeps
        // the occurrences.
        let empty = predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            "stale",
            &serde_json::json!({"fields": {"/event": "absent"}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(empty.occurrences.len(), 1);
        assert!(empty.matches.is_empty());
    }

    #[test]
    fn predicate_queries_scope_service_on_needle_and_events() {
        let result = predicate_fixture();
        // The counter filter keeps the stale needle (the delta is on
        // counter) but every later boundary is api, so the boundary
        // service filter empties the matches.
        let scoped = predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            "stale",
            &serde_json::json!({"fields": {"/event": "request"}}),
            Some("counter"),
            None,
        )
        .unwrap();
        assert_eq!(scoped.occurrences.len(), 1);
        assert!(scoped.matches.is_empty());

        // The api filter keeps the needle occurrences (the delta is on
        // counter, so it disappears) - assert the negative instead: an api
        // needle prints nowhere in this fixture.
        let api_needle = predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            "stale",
            &serde_json::json!({"fields": {"/event": "request"}}),
            Some("api"),
            None,
        )
        .unwrap();
        assert!(api_needle.occurrences.is_empty());
        assert!(api_needle.matches.is_empty());
    }

    #[test]
    fn event_predicates_accept_the_shared_grammar() {
        let result = serde_json::json!({
            "runs": [{"index": 0, "timeline": [
                {"id": "op-000-write", "operation": "write", "service": "api",
                 "moment": "7000@input-hash",
                 "events": {"api": [
                     "{\"event\":\"write\",\"seq\":1,\"tag\":\"retry-7\",\"steps\":[{\"kind\":\"go\"},{\"kind\":\"go\"}]}"
                 ]}},
                {"id": "op-001-read", "operation": "read", "service": "counter",
                 "moment": "9000@read-hash",
                 "events": {"counter": ["{\"event\":\"read\",\"seq\":2}"]}}
            ]}]
        });

        // A where condition with comparisons, a regex, and existence,
        // beside fields and array quantifiers.
        let rich = serde_json::json!({
            "fields": {"/event": "write"},
            "where": [
                {"pointer": "/seq", "greater_than_or_equal": 1, "less_than": 2},
                {"pointer": "/tag", "matches": "^retry-\\d+$"},
                {"pointer": "/ack", "exists": false}
            ],
            "arrays": [{"pointer": "/steps",
                        "all": {"fields": {"/kind": "go"}},
                        "none": {"fields": {"/kind": "stop"}}}]
        });
        let query =
            event_temporal_query(&result, TemporalRelation::PrecededBy, &rich, None, None).unwrap();
        assert_eq!(query.occurrences.len(), 1);
        assert_eq!(query.matches.len(), 1);
        assert_eq!(query.matches[0].boundary, "op-001-read");

        // A JSONPath query selects inside the event.
        let path = serde_json::json!({
            "query": "$.steps[?@.kind == \"go\"]",
            "fields": {"/event": "write"}
        });
        let pathed =
            event_temporal_query(&result, TemporalRelation::PrecededBy, &path, None, None).unwrap();
        assert_eq!(pathed.occurrences.len(), 1);

        // Nested any/none lists compose.
        let nested = serde_json::json!({
            "all": [{"fields": {"/event": "write"}}],
            "any": [{"fields": {"/seq": 9}}, {"fields": {"/seq": 1}}],
            "none": [{"fields": {"/tag": "final"}}]
        });
        let nested_query =
            event_temporal_query(&result, TemporalRelation::PrecededBy, &nested, None, None)
                .unwrap();
        assert_eq!(nested_query.occurrences.len(), 1);
    }

    #[test]
    fn event_predicates_reject_grammar_violations_by_name() {
        let result = temporal_fixture();
        let cases = [
            // Capture keys cannot mean anything on a one-event query.
            serde_json::json!({"fields": {"/a": 1}, "capture": {"x": "/a"}}),
            serde_json::json!({"fields": {"/a": 1}, "equals_capture": {"/a": "x"}}),
            serde_json::json!({"unknown": 1}),
            serde_json::json!({"where": [{"pointer": "no-slash", "equals": 1}]}),
            serde_json::json!({"where": [{"pointer": "/a", "between": 1}]}),
            serde_json::json!({"arrays": [{"any": {"fields": {"/x": 1}}}]}),
            serde_json::json!({"all": [{"fields": {}}]}),
            serde_json::json!({"query": 3}),
        ];
        for predicate in cases {
            let error = event_temporal_query(
                &result,
                TemporalRelation::PrecededBy,
                &predicate,
                None,
                None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("event predicate"), "{error}");
        }
    }

    #[test]
    fn event_predicate_queries_bind_two_predicates_on_distinct_boundaries() {
        // The stale marker event prints on the counter boundary; the
        // retry request prints on the final api boundary after it.
        let result = serde_json::json!({
            "runs": [{"index": 0, "timeline": [
                {"id": "op-000-write", "operation": "write", "service": "api",
                 "moment": "7000@input-hash",
                 "events": {"api": ["{\"event\":\"request\",\"seq\":1}"]}},
                {"id": "op-001-read", "operation": "read", "service": "counter",
                 "moment": "9000@read-hash",
                 "events": {"counter": ["{\"event\":\"stale\",\"seq\":2}"]}},
                {"id": "op-002-verify", "operation": "verify", "service": "api",
                 "moment": "12000@verify-hash",
                 "events": {"api": ["{\"event\":\"verify\",\"seq\":3}"]}},
                {"id": "op-003-final", "operation": "final", "service": "api",
                 "moment": "15000@final-hash",
                 "events": {"api": ["{\"event\":\"request\",\"retry\":true}"]}}
            ]}]
        });

        let query = event_predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            &serde_json::json!({"fields": {"/event": "stale"}}),
            &serde_json::json!({"fields": {"/event": "request", "/retry": true}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(query.format, "theseus-query-event-predicate-v1");
        assert_eq!(query.relation, "preceded_by");
        assert_eq!(query.occurrences.len(), 1);
        assert_eq!(query.occurrences[0].service, "counter");
        assert_eq!(query.occurrences[0].boundary, "op-001-read");
        // The write boundary carries a request without retry, the verify
        // boundary a verify event: only the final boundary satisfies the
        // where-predicate.
        assert_eq!(query.matches.len(), 1);
        assert_eq!(query.matches[0].boundary, "op-003-final");

        // The same boundary may satisfy both predicates.
        let same = event_predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            &serde_json::json!({"fields": {"/event": "stale"}}),
            &serde_json::json!({"fields": {"/event": "stale"}}),
            None,
            None,
        )
        .unwrap();
        assert!(same.matches.is_empty());

        let followed = event_predicate_query(
            &result,
            TemporalRelation::FollowedBy,
            &serde_json::json!({"fields": {"/event": "stale"}}),
            &serde_json::json!({"fields": {"/event": "request"}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(followed.matches.len(), 1);
        assert_eq!(followed.matches[0].boundary, "op-000-write");

        // A where-predicate nothing satisfies keeps the occurrences.
        let empty = event_predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            &serde_json::json!({"fields": {"/event": "stale"}}),
            &serde_json::json!({"fields": {"/event": "absent"}}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(empty.occurrences.len(), 1);
        assert!(empty.matches.is_empty());
    }

    #[test]
    fn event_predicate_queries_reject_malformed_predicates() {
        let result = temporal_fixture();
        for (relation_predicate, where_predicate) in [
            (
                serde_json::json!({}),
                serde_json::json!({"fields": {"/a": 1}}),
            ),
            (
                serde_json::json!({"fields": {}}),
                serde_json::json!({"fields": {"/a": 1}}),
            ),
            (
                serde_json::json!({"fields": {"/a": 1}}),
                serde_json::json!({"fields": {"a": 1}}),
            ),
            (
                serde_json::json!({"fields": {"/a": 1}}),
                serde_json::json!("stale"),
            ),
        ] {
            let error = event_predicate_query(
                &result,
                TemporalRelation::PrecededBy,
                &relation_predicate,
                &where_predicate,
                None,
                None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("event predicate"), "{error}");
        }
    }

    #[test]
    fn predicate_queries_reject_malformed_input() {
        let result = predicate_fixture();
        assert!(predicate_query(
            &result,
            TemporalRelation::PrecededBy,
            "",
            &serde_json::json!({"fields": {"/a": 1}}),
            None,
            None
        )
        .unwrap_err()
        .to_string()
        .contains("non-empty needle"));
        for predicate in [
            serde_json::json!("request"),
            serde_json::json!({}),
            serde_json::json!({"fields": {}}),
            serde_json::json!({"fields": {"event": "request"}}),
        ] {
            let error = predicate_query(
                &result,
                TemporalRelation::PrecededBy,
                "stale",
                &predicate,
                None,
                None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("event predicate"), "{error}");
        }
    }

    /// A one-run bundle whose second boundary carries a verified cumulative
    /// serial hash and a decision trace with a template header.
    fn collect_fixture(bundle: &Path, transcript: &[u8]) {
        use sha2::{Digest, Sha256};
        let cumulative = format!("{:x}", Sha256::digest(transcript));
        let early = format!("{:x}", Sha256::digest(&transcript[..5]));
        let directory = bundle;
        fs::create_dir_all(directory.join("runs/000/services/counter")).unwrap();
        fs::write(directory.join("campaign-result.json"), format!(r#"{{"runs":[{{"index":0,"structured_choices":{{"api":[{{"ordinal":0,"name":"mode","upper_exclusive":2,"selected":1}}]}},"decision_trace":["test_template:main","boundary:0:operation:write","boundary:1:operation:read"],"timeline":[
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
        fs::write(
            directory.join("progress.jsonl"),
            concat!(
                r#"{"format":"theseus-progress-v1","completed":1,"index":0,"status":"passed"}"#,
                "\n",
                r#"{"format":"theseus-run-record-v1","index":0,"status":"passed","operations":["write"]}"#,
                "\n",
                r#"{"format":"theseus-checkpoint-ledger-v1","nodes":1,"reuses":0}"#,
                "\n",
                r#"{"format":"theseus-progress-v1","completed":2,"index":1,"status":"failed"}"#,
                "\n",
            ),
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
        // The journal prefix covers the collected run only: three lines up
        // to and including run 0's ledger, excluding run 1's progress line.
        let journal = fs::read_to_string(output.join("progress.jsonl")).unwrap();
        assert_eq!(journal.lines().count(), 3);
        assert!(journal.contains("theseus-checkpoint-ledger-v1"));
        assert!(!journal.contains(r#""completed":2"#));

        let choices: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("choices.json")).unwrap()).unwrap();
        assert_eq!(choices["format"], "theseus-collected-choices-v1");
        assert_eq!(choices["run"], 0);
        assert_eq!(choices["structured_choices"]["api"][0]["name"], "mode");

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
        assert_eq!(collected.files.len(), 6);
        assert!(output.join("progress.jsonl").is_file());
        assert!(output.join("choices.json").is_file());
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
        assert_eq!(collected.files.len(), 5);
        assert!(output.join("progress.jsonl").is_file());
        assert!(output.join("choices.json").is_file());

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
