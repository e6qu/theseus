// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A read-only HTTP surface for retained campaign bundles.
//!
//! [`serve_campaigns`] binds one local address and answers GET requests for
//! the named campaigns' retained evidence: the versioned result and replay
//! plan, a rendered markdown report, and the serial logs the result
//! references. Every other method is refused, nothing is ever written, and
//! serial routes cannot escape the bundle's `serial/` directory.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// One served bundle: its display name, its kind (the versioned result it
/// retains), and its directory.
#[derive(Debug)]
pub(crate) struct ServedCampaign {
    name: String,
    kind: &'static str,
    root: PathBuf,
}

/// Serve retained campaign bundles over local read-only HTTP. Blocks while
/// connections arrive; the process ends when the caller interrupts it.
pub fn serve_campaigns(sources: &[PathBuf], address: &str) -> Result<(), String> {
    serve_campaign_list(collect_campaigns(sources)?, address)
}

/// Serve the bundles a versioned registry manifest names, in registry
/// order. Registry directories resolve from the registry file's directory.
pub fn serve_registry(index: &Path, address: &str) -> Result<(), String> {
    serve_campaign_list(collect_registry(index)?, address)
}

fn serve_campaign_list(campaigns: Vec<ServedCampaign>, address: &str) -> Result<(), String> {
    let listener =
        TcpListener::bind(address).map_err(|error| format!("cannot bind {address}: {error}"))?;
    eprintln!(
        "theseus serving {} bundle(s) at http://{address}",
        campaigns.len()
    );
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = handle_connection(&mut stream, &campaigns);
    }
    Ok(())
}

/// The bundle kind a retained versioned result implies.
fn bundle_kind(root: &Path) -> &'static str {
    match retained_result(root) {
        Some("campaign-result.json") => "campaign",
        Some("result.json") => "exploration",
        Some("topology-result.json") => "topology",
        _ => "unknown",
    }
}

/// Read a versioned serve registry: named bundles (`name` -> `directory`
/// pairs) with directories resolving from the registry file's directory.
pub fn collect_registry(index: &Path) -> Result<Vec<ServedCampaign>, String> {
    let index =
        std::fs::canonicalize(index).map_err(|error| format!("{}: {error}", index.display()))?;
    let base = index
        .parent()
        .ok_or_else(|| format!("{}: registry has no directory", index.display()))?;
    let bytes = std::fs::read(&index).map_err(|error| format!("{}: {error}", index.display()))?;
    let registry: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", index.display()))?;
    if registry["format"] != "theseus-serve-registry-v1" {
        return Err(format!(
            "{}: registry format must be theseus-serve-registry-v1",
            index.display()
        ));
    }
    let entries = registry["campaigns"]
        .as_array()
        .ok_or_else(|| format!("{}: registry needs a campaigns array", index.display()))?;
    let mut campaigns = Vec::with_capacity(entries.len());
    let mut names = std::collections::BTreeSet::new();
    for entry in entries {
        let name = entry["name"]
            .as_str()
            .ok_or_else(|| format!("{}: registry entry needs a name", index.display()))?
            .to_owned();
        let directory = entry["directory"]
            .as_str()
            .ok_or_else(|| {
                format!(
                    "{}: registry entry {name:?} needs a directory",
                    index.display()
                )
            })?
            .to_owned();
        let root = std::fs::canonicalize(base.join(&directory))
            .map_err(|error| format!("registry entry {name:?}: {directory}: {error}"))?;
        if retained_result(&root).is_none() {
            return Err(format!(
                "registry entry {name:?}: {directory} retains no versioned result"
            ));
        }
        if !names.insert(name.clone()) {
            return Err(format!("registry entry {name:?} duplicates a name"));
        }
        campaigns.push(ServedCampaign {
            name,
            kind: bundle_kind(&root),
            root,
        });
    }
    if campaigns.is_empty() {
        return Err(format!("{}: registry names no bundles", index.display()));
    }
    Ok(campaigns)
}

fn collect_campaigns(sources: &[PathBuf]) -> Result<Vec<ServedCampaign>, String> {
    if sources.is_empty() {
        return Err("serve needs at least one campaign bundle".to_owned());
    }
    let mut campaigns = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for source in sources {
        let root = std::fs::canonicalize(source)
            .map_err(|error| format!("{}: {error}", source.display()))?;
        if root.is_file() {
            return Err(format!(
                "{}: serve names campaign bundles or directories of them",
                root.display()
            ));
        }
        if retained_result(&root).is_some() {
            push_campaign(&mut campaigns, &mut names, root)?;
            continue;
        }
        let mut children: Vec<PathBuf> = std::fs::read_dir(&root)
            .map_err(|error| format!("{}: {error}", root.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir() && retained_result(path).is_some())
            .collect();
        children.sort();
        if children.is_empty() {
            return Err(format!(
                "{}: no bundles found; a served bundle retains a versioned result (campaign, exploration, or topology)",
                root.display()
            ));
        }
        for child in children {
            push_campaign(&mut campaigns, &mut names, child)?;
        }
    }
    Ok(campaigns)
}

fn push_campaign(
    campaigns: &mut Vec<ServedCampaign>,
    names: &mut std::collections::BTreeSet<String>,
    root: PathBuf,
) -> Result<(), String> {
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{}: bundle has no directory name", root.display()))?;
    if !names.insert(name.clone()) {
        return Err(format!(
            "two bundles share the name {name}; serve campaigns with distinct directory names"
        ));
    }
    let kind = bundle_kind(&root);
    campaigns.push(ServedCampaign { name, kind, root });
    Ok(())
}

fn handle_connection(stream: &mut TcpStream, campaigns: &[ServedCampaign]) -> std::io::Result<()> {
    let request = read_request(stream)?;
    let Some((method, path)) = parse_request(&request) else {
        return write_response(
            stream,
            400,
            "text/plain; charset=utf-8",
            b"malformed request",
        );
    };
    if method != "GET" {
        return write_response(
            stream,
            405,
            "text/plain; charset=utf-8",
            b"method not allowed; this surface is read-only",
        );
    }
    let (status, content_type, body) = route(campaigns, &path);
    write_response(stream, status, content_type, &body)
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk)?;
        buffer.extend_from_slice(&chunk[..read]);
        if read == 0 || buffer.len() > 8192 || buffer.windows(4).any(|window| window == b"\r\n\r\n")
        {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn parse_request(request: &str) -> Option<(String, String)> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    Some((method, path))
}

fn route(campaigns: &[ServedCampaign], path: &str) -> (u16, &'static str, Vec<u8>) {
    let path = path.split('#').next().unwrap_or("/");
    if path == "/" || path.is_empty() {
        return (200, "text/html; charset=utf-8", index_page(campaigns));
    }
    let trimmed = path.trim_start_matches('/');
    if let Some(history) = trimmed.strip_prefix("history/") {
        return route_history(campaigns, history);
    }
    if trimmed.split('?').next() == Some("compare") {
        let query = trimmed
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or("");
        return route_compare(campaigns, query);
    }
    let (name, rest) = match trimmed.split_once('/') {
        Some((name, rest)) => (name, rest),
        None => (trimmed, ""),
    };
    // Query routes keep their `?parameters`; the verbatim evidence routes
    // ignore anything after `?`.
    let rest = if rest.starts_with("query/") {
        rest
    } else {
        rest.split('?').next().unwrap_or_default()
    };
    let Some(campaign) = campaigns.iter().find(|campaign| campaign.name == name) else {
        return not_found();
    };
    if rest == "result" {
        let Some(name) = retained_result(&campaign.root) else {
            return not_found();
        };
        return read_file(&campaign.root.join(name), "application/json");
    }
    if rest == "plan" {
        return read_file(&campaign.root.join("replay-plan.json"), "application/json");
    }
    if rest == "report" {
        return match crate::report::report_text(
            &campaign.root,
            crate::report::ReportFormat::Markdown,
        ) {
            Ok(markdown) => (200, "text/markdown; charset=utf-8", markdown.into_bytes()),
            Err(_) => (
                500,
                "text/plain; charset=utf-8",
                b"report rendering failed".to_vec(),
            ),
        };
    }
    if let Some(relative) = rest.strip_prefix("serial/") {
        return serve_serial(&campaign.root, relative);
    }
    if let Some(query) = rest.strip_prefix("query/") {
        return route_query(campaign, query);
    }
    if rest == "tree" {
        return route_tree(campaign);
    }
    if rest == "progress" {
        return read_file(
            &campaign.root.join("progress.jsonl"),
            "text/plain; charset=utf-8",
        );
    }
    if let Some(relative) = rest.strip_prefix("file/") {
        return route_file(campaign, relative);
    }
    not_found()
}

/// The retained-file route: any file the tree lists, with the content type
/// implied by the extension. Symlinks are refused like the tree skips
/// them, and path segments cannot escape the bundle.
fn route_file(campaign: &ServedCampaign, relative: &str) -> (u16, &'static str, Vec<u8>) {
    if relative
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return not_found();
    }
    let path = campaign.root.join(relative);
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return not_found();
    };
    if metadata.is_symlink() || !metadata.is_file() {
        return not_found();
    }
    let content_type = if relative.ends_with(".json") {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    read_file(&path, content_type)
}

/// The bundle tree route: every retained file, relative path and byte
/// size, sorted, so the serial logs and artifacts the evidence references
/// are reachable without knowing their names.
fn route_tree(campaign: &ServedCampaign) -> (u16, &'static str, Vec<u8>) {
    let mut files = Vec::new();
    if collect_tree(&campaign.root, &campaign.root, 0, &mut files).is_err() {
        return (
            500,
            "text/plain; charset=utf-8",
            b"bundle unreadable".to_vec(),
        );
    }
    files.sort();
    let listing = files
        .into_iter()
        .map(|(path, bytes)| serde_json::json!({"path": path, "bytes": bytes}))
        .collect::<Vec<_>>();
    json_response(&serde_json::json!({
        "format": "theseus-bundle-tree-v1",
        "files": listing,
    }))
}

/// Walk a bundle's retained files as sorted-relative paths. Symlinks are
/// skipped (neither followed nor listed) so a bundle cannot point outside
/// itself; depth and count stay bounded.
fn collect_tree(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut Vec<(String, u64)>,
) -> std::io::Result<()> {
    if depth > 8 || files.len() > 4096 {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_tree(root, &path, depth + 1, files)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let bytes = entry.metadata()?.len();
            files.push((relative, bytes));
        }
    }
    Ok(())
}

/// The comparison route: the committed guidance-comparison artifact over
/// named served campaigns, under the same corpus and budget rules the CLI
/// enforces.
fn route_compare(campaigns: &[ServedCampaign], query: &str) -> (u16, &'static str, Vec<u8>) {
    let Some(names) = query_parameter(query, "campaigns") else {
        return (
            400,
            "text/plain; charset=utf-8",
            b"compare needs ?campaigns=name,name".to_vec(),
        );
    };
    let mut sources = Vec::new();
    for name in names.split(',') {
        let Some(campaign) = campaigns.iter().find(|campaign| campaign.name == name) else {
            return not_found();
        };
        sources.push(campaign.root.clone());
    }
    if sources
        .iter()
        .any(|source| !source.join("campaign-result.json").is_file())
    {
        return (
            400,
            "text/plain; charset=utf-8",
            b"compare names campaign bundles only".to_vec(),
        );
    }
    match crate::evaluation::evaluate_compare(&sources) {
        Ok(comparison) => match serde_json::to_string_pretty(&comparison) {
            Ok(text) => (200, "application/json", text.into_bytes()),
            Err(_) => (
                500,
                "text/plain; charset=utf-8",
                b"serialization failed".to_vec(),
            ),
        },
        Err(crate::evaluation::EvaluationError::Invalid(message)) => {
            (400, "text/plain; charset=utf-8", message.into_bytes())
        }
        Err(_) => (
            500,
            "text/plain; charset=utf-8",
            b"comparison failed".to_vec(),
        ),
    }
}

/// The served bundles that retain campaign evidence: the history
/// aggregations' input.
fn campaign_sources(campaigns: &[ServedCampaign]) -> Vec<std::path::PathBuf> {
    campaigns
        .iter()
        .filter(|campaign| campaign.root.join("campaign-result.json").is_file())
        .map(|campaign| campaign.root.clone())
        .collect()
}

/// The cross-campaign history routes: property, assertion, and event
/// aggregations over the whole served set, answered by the same functions
/// the CLI's `theseus history` uses.
fn route_history(campaigns: &[ServedCampaign], rest: &str) -> (u16, &'static str, Vec<u8>) {
    let (route, query) = rest.split_once('?').unwrap_or((rest, ""));
    let service = query_parameter(query, "service").map(str::to_owned);
    let property = query_parameter(query, "property").map(str::to_owned);
    let sources: Vec<std::path::PathBuf> = campaign_sources(campaigns);
    if sources.is_empty() {
        return empty_history(route).unwrap_or_else(not_found);
    }
    if route == "properties" {
        return answer(crate::history::property_history(
            &sources,
            property.as_deref(),
        ));
    }
    if route == "assertions" {
        return answer(crate::history::assertion_catalog(&sources));
    }
    if route == "events" {
        return answer(crate::history::event_history(&sources, service.as_deref()));
    }
    not_found()
}

/// The exploration node routes: the retained search tree, listed and
/// resolved by seed path, with the replay and minimize commands the CLI
/// builds.
fn route_nodes(campaign: &ServedCampaign, rest: &str) -> (u16, &'static str, Vec<u8>) {
    let (route, _) = rest.split_once('?').unwrap_or((rest, ""));
    let bytes = match std::fs::read(campaign.root.join("result.json")) {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                500,
                "text/plain; charset=utf-8",
                b"exploration result unreadable".to_vec(),
            )
        }
    };
    let result = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(result) => result,
        Err(_) => {
            return (
                500,
                "text/plain; charset=utf-8",
                b"exploration result unparsable".to_vec(),
            )
        }
    };
    let mut nodes = result["nodes"].as_array().cloned().unwrap_or_default();
    nodes.sort_by_key(|node| node["search_index"].as_u64().unwrap_or(0));
    if route == "nodes" {
        let shaped = nodes
            .iter()
            .map(|node| served_node(&campaign.root, node))
            .collect::<Vec<_>>();
        return json_response(&serde_json::json!({
            "format": "theseus-exploration-nodes-v1",
            "nodes": shaped,
        }));
    }
    if let Some(seed_path) = route.strip_prefix("node/") {
        let wanted = seed_path
            .split(',')
            .filter_map(|part| part.trim().parse::<u64>().ok())
            .collect::<Vec<_>>();
        for node in &nodes {
            let path = node["seed_path"]
                .as_array()
                .map(|path| {
                    path.iter()
                        .filter_map(|value| value.as_u64())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !wanted.is_empty() && path == wanted {
                return json_response(&served_node(&campaign.root, node));
            }
        }
        return not_found();
    }
    not_found()
}

fn served_node(root: &Path, node: &serde_json::Value) -> serde_json::Value {
    let seed_path = node["seed_path"]
        .as_array()
        .map(|path| {
            path.iter()
                .filter_map(|value| value.as_u64())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let joined = seed_path
        .iter()
        .map(|seed| seed.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut record = serde_json::json!({
        "search_index": node["search_index"],
        "seed_path": seed_path,
        "entropy_probe_hex": node["entropy_probe_hex"],
        "markers_hex": node["markers_hex"],
        "dirty_pages": node["dirty_pages"],
        "replay_command": format!(
            "theseus explore --replay {} --seed-path {}",
            root.display(),
            joined
        ),
        "minimize_command": format!(
            "theseus explore --minimize {} --seed-path {}",
            root.display(),
            joined
        ),
    });
    if let Some(serial_log) = node["serial_log"].as_str() {
        record["serial_log"] = serde_json::json!(serial_log);
    }
    record
}

fn json_response(value: &serde_json::Value) -> (u16, &'static str, Vec<u8>) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => (200, "application/json", text.into_bytes()),
        Err(_) => (
            500,
            "text/plain; charset=utf-8",
            b"serialization failed".to_vec(),
        ),
    }
}

/// The empty history shapes, answered when no served bundle retains
/// campaign evidence.
fn empty_history(route: &str) -> Option<(u16, &'static str, Vec<u8>)> {
    use crate::history::{AssertionCatalog, CampaignPropertyHistory, EventHistory};
    let json = match route {
        "properties" => serde_json::to_string_pretty(&CampaignPropertyHistory {
            format: "theseus-campaign-property-history-v1",
            sources: Vec::new(),
            properties: Vec::new(),
        })
        .ok()?,
        "assertions" => serde_json::to_string_pretty(&AssertionCatalog {
            format: "theseus-assertion-catalog-v1",
            sources: Vec::new(),
            assertions: Vec::new(),
        })
        .ok()?,
        "events" => serde_json::to_string_pretty(&EventHistory {
            format: "theseus-event-history-v1",
            sources: Vec::new(),
            events: Vec::new(),
        })
        .ok()?,
        _ => return None,
    };
    Some((200, "application/json", json.into_bytes()))
}

/// The read-only query routes: the campaign's moment index, its guest event
/// records, and the needle relations, all answered from the retained result.
fn route_query(campaign: &ServedCampaign, rest: &str) -> (u16, &'static str, Vec<u8>) {
    if campaign.root.join("result.json").is_file() {
        return route_nodes(campaign, rest);
    }
    let (route, query) = rest.split_once('?').unwrap_or((rest, ""));
    let service = query_parameter(query, "service").map(str::to_owned);
    let bytes = match std::fs::read(campaign.root.join("campaign-result.json")) {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                500,
                "text/plain; charset=utf-8",
                b"campaign result unreadable".to_vec(),
            )
        }
    };
    let result = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(result) => result,
        Err(_) => {
            return (
                500,
                "text/plain; charset=utf-8",
                b"campaign result unparsable".to_vec(),
            )
        }
    };
    if route == "moments" {
        return answer(crate::query::list_moments(&result, service.as_deref()));
    }
    if let Some(moment) = route.strip_prefix("moment/") {
        let moment = percent_decode(moment);
        let next = query_flag(query, "next");
        let previous = query_flag(query, "previous");
        if next && previous {
            return (
                400,
                "text/plain; charset=utf-8",
                b"choose either ?next or ?previous".to_vec(),
            );
        }
        let resolved = if next {
            crate::query::next_moment_in(&result, &moment)
        } else if previous {
            crate::query::previous_moment_in(&result, &moment)
        } else {
            crate::query::find_moment(&result, &moment)
        };
        return answer(resolved);
    }
    if route == "events" {
        return answer(crate::query::list_events(&result, service.as_deref()));
    }
    let relation = if let Some(needle) = route.strip_prefix("preceded-by/") {
        Some((crate::query::TemporalRelation::PrecededBy, needle))
    } else if let Some(needle) = route.strip_prefix("followed-by/") {
        Some((crate::query::TemporalRelation::FollowedBy, needle))
    } else {
        None
    };
    if let Some((relation, needle)) = relation {
        return answer(crate::query::temporal_query(
            &result,
            relation,
            &percent_decode(needle),
            service.as_deref(),
        ));
    }
    not_found()
}

/// Not-found-shaped errors from the query and history layers. History has
/// no not-found shape: every failure is a failed aggregation, except a
/// named source that retains no evidence.
enum QueryFailure {
    NotFound,
    Failed,
}

impl From<crate::query::MomentError> for QueryFailure {
    fn from(error: crate::query::MomentError) -> Self {
        match error {
            crate::query::MomentError::NotFound(_) => Self::NotFound,
            _ => Self::Failed,
        }
    }
}

impl From<crate::history::HistoryError> for QueryFailure {
    fn from(_error: crate::history::HistoryError) -> Self {
        Self::Failed
    }
}

fn answer<T: serde::Serialize, E: Into<QueryFailure>>(
    value: Result<T, E>,
) -> (u16, &'static str, Vec<u8>) {
    match value.map_err(Into::into) {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(text) => (200, "application/json", text.into_bytes()),
            Err(_) => (
                500,
                "text/plain; charset=utf-8",
                b"serialization failed".to_vec(),
            ),
        },
        Err(QueryFailure::NotFound) => not_found(),
        Err(QueryFailure::Failed) => (500, "text/plain; charset=utf-8", b"query failed".to_vec()),
    }
}

/// Decode the one percent-encoded path segment a needle route carries.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 3 <= bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                decoded.push(byte);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn query_parameter<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn query_flag(query: &str, name: &str) -> bool {
    query.split('&').any(|pair| pair == name)
}

/// The versioned result a bundle retains: a campaign result, a
/// single-timeline or exploration result, or a topology result.
fn retained_result(root: &Path) -> Option<&'static str> {
    for name in [
        "campaign-result.json",
        "result.json",
        "topology-result.json",
    ] {
        if root.join(name).is_file() {
            return Some(name);
        }
    }
    None
}

fn serve_serial(root: &Path, relative: &str) -> (u16, &'static str, Vec<u8>) {
    if relative
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return not_found();
    }
    read_file(
        &root.join("serial").join(relative),
        "text/plain; charset=utf-8",
    )
}

fn read_file(path: &Path, content_type: &'static str) -> (u16, &'static str, Vec<u8>) {
    match std::fs::read(path) {
        Ok(bytes) => (200, content_type, bytes),
        Err(_) => not_found(),
    }
}

fn not_found() -> (u16, &'static str, Vec<u8>) {
    (404, "text/plain; charset=utf-8", b"not found".to_vec())
}

fn index_page(campaigns: &[ServedCampaign]) -> Vec<u8> {
    let mut page = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>theseus campaigns</title></head><body><h1>Retained campaigns</h1><ul>",
    );
    for campaign in campaigns {
        let name = escape_html(&campaign.name);
        let plan_link = campaign
            .root
            .join("replay-plan.json")
            .is_file()
            .then(|| format!(" · <a href=\"/{name}/plan\">plan</a>"))
            .unwrap_or_default();
        page.push_str(&format!(
            "<li>{} · <a href=\"/{name}/report\">{name}</a> · <a href=\"/{name}/result\">result</a>{plan_link}</li>",
            campaign.kind
        ));
    }
    page.push_str("</ul></body></html>");
    page.into_bytes()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn write_bundle(directory: &Path) -> PathBuf {
        write_named_bundle(directory, "campaign", "unified", 12)
    }

    fn write_named_bundle(
        directory: &Path,
        name: &str,
        guidance: &str,
        candidates: u64,
    ) -> PathBuf {
        let bundle = directory.join(name);
        fs::create_dir_all(bundle.join("serial")).unwrap();
        fs::write(
            bundle.join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","campaign":{"driver":"api","operations":[{"name":"calculate"}],"max_runs":8}}"#,
        )
        .unwrap();
        let result = r#"{"format":"theseus-compose-campaign-result-v1","status":"failed","driver":"chooser","guidance":"GUIDANCE","generated_candidates":CANDIDATES,"structured_choice_decisions":1,"runs":[{"index":0,"operations":["calculate[mode-1]"],"status":"failed","structured_choices":{"chooser":[{"ordinal":0,"name":"mode","upper_exclusive":2,"selected":1}]},"timeline":[{"id":"op-000-calculate","operation":"calculate[mode-1]","service":"chooser","round":7,"markers":["42"],"new_markers":["42"],"serial_delta":{"chooser":{"bytes":16,"sha256":"delta-hash","excerpt":"calculate ready\n","omitted_bytes":0}},"state_sha256":"state-hash","moment":"7000@input-hash","events":{"chooser":["{\"event\":\"request\",\"seq\":1}"]}},{"id":"op-001-calculate","operation":"calculate[mode-1]","service":"chooser","round":9,"markers":["42","a1"],"new_markers":["a1"],"serial_delta":{"chooser":{"bytes":11,"sha256":"tail-hash","excerpt":"calculate done\n","omitted_bytes":0}},"state_sha256":"tail-state","moment":"9000@input-hash"}]}],"properties":[{"name":"consistent_read","kind":"always","status":"passed","detail":"2 of 2 retained timelines contained pass"}]}"#;
        let result = result
            .replace("GUIDANCE", guidance)
            .replace("CANDIDATES", &candidates.to_string());
        fs::write(bundle.join("campaign-result.json"), result).unwrap();
        fs::write(bundle.join("serial").join("1.log"), b"ready\n").unwrap();
        fs::write(
            bundle.join("progress.jsonl"),
            concat!(
                r#"{"format":"theseus-progress-v1","completed":1,"index":0,"status":"passed"}"#,
                "\n",
                r#"{"format":"theseus-run-record-v1","index":0,"status":"passed","operations":["write"],"faults":[],"selection":"first"}"#,
                "\n",
                r#"{"format":"theseus-progress-v1","completed":2,"index":1,"status":"failed"}"#,
                "\n",
                r#"{"format":"theseus-run-record-v1","index":1,"status":"failed","operations":["read"],"faults":["backplane:partition@read"],"selection":"extends 1-operation prefix"}"#,
                "\n",
                r#"{"format":"theseus-checkpoint-ledger-v1","nodes":4,"reuses":3,"prefix_captures":2,"prefix_restores":1,"retained_memory_bytes":1048576}"#,
                "\n",
            ),
        )
        .unwrap();
        bundle
    }

    fn write_exploration_bundle(directory: &Path, name: &str) -> PathBuf {
        let bundle = directory.join(name);
        fs::create_dir_all(bundle.join("serial")).unwrap();
        fs::write(
            bundle.join("explore-plan.json"),
            r#"{"format":"theseus-explore-plan-v1","max_depth":2}"#,
        )
        .unwrap();
        fs::write(
            bundle.join("result.json"),
            r#"{"format":"theseus-result-v1","status":"passed","checks":[],"nodes":[{"search_index":1,"id":2,"parent":null,"depth":1,"seed":7,"seed_path":[1],"entropy_probe_hex":"aa","markers_hex":"ff","dirty_pages":2,"serial_log":"serial/1.log"},{"search_index":2,"id":3,"parent":2,"depth":2,"seed":8,"seed_path":[1,2],"entropy_probe_hex":"bb","markers_hex":"90ff","dirty_pages":3}]}"#,
        )
        .unwrap();
        fs::write(
            bundle.join("progress.jsonl"),
            concat!(
                r#"{"format":"theseus-node-record-v1","search_index":0,"seed":7,"seed_path":[7],"markers_hex":"ff","dirty_pages":2,"serial_bytes":6}"#,
                "\n",
                r#"{"format":"theseus-node-record-v1","search_index":1,"seed":8,"seed_path":[7,8],"markers_hex":"90ff","dirty_pages":3,"serial_bytes":6}"#,
                "\n",
            ),
        )
        .unwrap();
        fs::write(bundle.join("serial").join("1.log"), b"ready\n").unwrap();
        bundle
    }

    fn write_topology_bundle(directory: &Path, name: &str) -> PathBuf {
        let bundle = directory.join(name);
        fs::create_dir_all(bundle.join("services/api")).unwrap();
        fs::write(
            bundle.join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","compose":"/tmp/compose.yaml"}"#,
        )
        .unwrap();
        fs::write(
            bundle.join("topology-result.json"),
            r#"{"status":"passed"}"#,
        )
        .unwrap();
        fs::write(
            bundle.join("services").join("api").join("result.json"),
            r#"{"status":"passed","checks":[{"name":"guest_exit","status":"passed","detail":"ok"}]}"#,
        )
        .unwrap();
        bundle
    }

    fn exchange(address: &str, request: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status: u16 = response
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_default();
        let content_type = response
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-type"))
            .map(|line| line.splitn(2, ": ").nth(1).unwrap_or_default().to_owned())
            .unwrap_or_default();
        let body = response
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        (status, content_type, body)
    }

    #[test]
    fn serves_evidence_routes_and_refuses_everything_else() {
        let directory = tempfile::tempdir().unwrap();
        write_bundle(directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        write_named_bundle(directory.path(), "coverage", "coverage", 12);
        write_named_bundle(directory.path(), "drift", "unified", 13);
        write_exploration_bundle(directory.path(), "exploration");
        write_topology_bundle(directory.path(), "topology");
        let campaigns = collect_campaigns(&[
            directory.path().join("campaign"),
            directory.path().join("coverage"),
            directory.path().join("drift"),
            directory.path().join("exploration"),
            directory.path().join("topology"),
        ])
        .unwrap();
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok(mut stream) => {
                        let _ = handle_connection(&mut stream.0, &campaigns);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let (status, content_type, body) =
            exchange(&address, "GET /campaign/result HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(
            body.contains("theseus-compose-campaign-result-v1"),
            "{body}"
        );

        let (status, content_type, body) =
            exchange(&address, "GET /campaign/plan HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("theseus-compose-plan-v1"), "{body}");

        let (status, content_type, body) =
            exchange(&address, "GET /campaign/report HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/markdown; charset=utf-8");
        assert!(body.contains("1 structured choices"), "{body}");

        let (status, content_type, body) = exchange(
            &address,
            "GET /campaign/serial/1.log HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert_eq!(body, "ready\n");

        let (status, ..) = exchange(
            &address,
            "GET /campaign/serial/../campaign-result.json HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, ..) = exchange(
            &address,
            "POST /campaign/result HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 405);

        let (status, ..) = exchange(&address, "GET /nope/result HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 404);

        let (status, content_type, body) = exchange(&address, "GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/html; charset=utf-8");
        assert!(body.contains("/campaign/report"), "{body}");
        assert!(body.contains("/coverage/report"), "{body}");
        assert!(body.contains("/drift/report"), "{body}");

        let (status, content_type, body) = exchange(
            &address,
            "GET /campaign/query/moments HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("\"moment\": \"7000@input-hash\""), "{body}");

        let (status, content_type, body) = exchange(
            &address,
            "GET /campaign/query/events HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("\\\"event\\\":\\\"request\\\""), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /campaign/query/events?service=none HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(body.trim(), "[]");

        let (status, _, body) = exchange(
            &address,
            "GET /campaign/query/preceded-by/calculate HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("7000@input-hash"), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /campaign/query/preceded-by/nomatch HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"matches\": []"), "{body}");

        let (status, ..) = exchange(
            &address,
            "GET /campaign/query/nope HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, content_type, body) = exchange(
            &address,
            "GET /campaign/query/moment/7000@input-hash HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("op-000-calculate"), "{body}");
        assert!(body.contains("\"next\": \"9000@input-hash\""), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /campaign/query/moment/7000@input-hash?next HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("op-001-calculate"), "{body}");
        assert!(body.contains("calculate done"), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /campaign/query/moment/9000@input-hash?previous HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("op-000-calculate"), "{body}");

        let (status, ..) = exchange(
            &address,
            "GET /campaign/query/moment/9000@input-hash?next HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, ..) = exchange(
            &address,
            "GET /campaign/query/moment/1000@missing HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, ..) = exchange(
            &address,
            "GET /campaign/query/moment/7000@input-hash?next&previous HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 400);

        let (status, content_type, body) = exchange(
            &address,
            "GET /history/properties HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(
            body.contains("theseus-campaign-property-history-v1"),
            "{body}"
        );

        assert!(body.contains("consistent_read"), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /history/properties?property=consistent_read HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("consistent_read"), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /history/properties?property=missing HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"properties\": []"), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /history/assertions HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("theseus-assertion-catalog-v1"), "{body}");

        let (status, _, body) =
            exchange(&address, "GET /history/events HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert!(body.contains("theseus-event-history-v1"), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /history/events?service=none HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"events\": []"), "{body}");

        let (status, ..) = exchange(&address, "GET /history/nope HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 404);

        let (status, content_type, body) = exchange(
            &address,
            "GET /compare?campaigns=campaign,coverage HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("theseus-guidance-comparison-v1"), "{body}");
        assert!(body.contains("\"corpus\": 12"), "{body}");
        assert!(body.contains("\"budget\": 8"), "{body}");
        assert!(body.contains("\"guidance\": \"unified\""), "{body}");
        assert!(body.contains("\"guidance\": \"coverage\""), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /compare?campaigns=campaign,drift HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 400);
        assert!(
            body.contains("explored a corpus of 13 candidates"),
            "{body}"
        );

        let (status, ..) = exchange(&address, "GET /compare HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 400);

        let (status, ..) = exchange(
            &address,
            "GET /compare?campaigns=campaign,ghost HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, content_type, body) = exchange(
            &address,
            "GET /exploration/result HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("theseus-result-v1"), "{body}");

        let (status, content_type, body) = exchange(
            &address,
            "GET /exploration/report HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/markdown; charset=utf-8");
        assert!(body.contains("Timeline replay"), "{body}");

        let (status, content_type, body) =
            exchange(&address, "GET /topology/result HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, "{\"status\":\"passed\"}");

        let (status, content_type, body) =
            exchange(&address, "GET /topology/report HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/markdown; charset=utf-8");
        assert!(body.contains("Topology replay"), "{body}");

        let (status, content_type, body) = exchange(
            &address,
            "GET /exploration/query/nodes HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("theseus-exploration-nodes-v1"), "{body}");
        assert!(body.contains("theseus explore --replay"), "{body}");
        assert!(body.contains("--minimize"), "{body}");
        assert!(body.contains("serial/1.log"), "{body}");

        let (status, _, body) = exchange(
            &address,
            "GET /exploration/query/node/1,2 HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"dirty_pages\": 3"), "{body}");
        assert!(body.contains("theseus explore --minimize"), "{body}");

        let (status, ..) = exchange(
            &address,
            "GET /exploration/query/node/9,9 HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, ..) = exchange(
            &address,
            "GET /exploration/query/moments HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, content_type, body) =
            exchange(&address, "GET /campaign/tree HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("theseus-bundle-tree-v1"), "{body}");
        assert!(
            body.contains("\"path\": \"campaign-result.json\""),
            "{body}"
        );
        assert!(body.contains("\"path\": \"replay-plan.json\""), "{body}");
        assert!(
            body.contains("\"bytes\": 6,\n      \"path\": \"serial/1.log\""),
            "{body}"
        );
        let result_position = body.find("campaign-result.json").unwrap();
        let plan_position = body.find("replay-plan.json").unwrap();
        let serial_position = body.find("serial/1.log").unwrap();
        assert!(result_position < plan_position);
        assert!(plan_position < serial_position);

        let (status, content_type, body) = exchange(
            &address,
            "GET /exploration/tree HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(body.contains("explore-plan.json"), "{body}");
        assert!(body.contains("serial/1.log"), "{body}");

        let (status, content_type, body) = exchange(
            &address,
            "GET /campaign/file/serial/1.log HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert_eq!(body, "ready\n");

        let (status, content_type, body) = exchange(
            &address,
            "GET /campaign/file/campaign-result.json HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "application/json");
        assert!(
            body.contains("theseus-compose-campaign-result-v1"),
            "{body}"
        );

        let (status, ..) = exchange(
            &address,
            "GET /campaign/file/serial/../campaign-result.json HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, ..) = exchange(
            &address,
            "GET /campaign/file/nope.txt HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 404);

        let (status, content_type, body) = exchange(
            &address,
            "GET /campaign/progress HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert!(body.contains("\"completed\":1"), "{body}");
        assert!(body.contains("\"status\":\"failed\""), "{body}");
        assert!(body.contains("theseus-run-record-v1"), "{body}");
        assert!(body.contains("theseus-checkpoint-ledger-v1"), "{body}");
        assert_eq!(body.lines().count(), 5);

        let (status, content_type, body) = exchange(
            &address,
            "GET /exploration/progress HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert!(body.contains("theseus-node-record-v1"), "{body}");
        assert!(body.contains("\"seed_path\":[7,8]"), "{body}");
        assert!(body.contains("\"serial_bytes\":6"), "{body}");
        assert_eq!(body.lines().count(), 2);

        running.store(false, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[test]
    fn serve_discovers_campaigns_and_rejects_duplicates() {
        assert!(collect_campaigns(&[]).is_err());
        let directory = tempfile::tempdir().unwrap();

        let bundle = write_named_bundle(directory.path(), "dupe", "unified", 12);
        let served = collect_campaigns(&[bundle]).unwrap();
        assert_eq!(served[0].name, "dupe");

        let other = write_named_bundle(&directory.path().join("nested"), "dupe", "coverage", 12);
        assert!(collect_campaigns(&[directory.path().join("dupe"), other]).is_err());

        let root = directory.path().join("guidance");
        write_named_bundle(&root, "unified", "unified", 12);
        write_named_bundle(&root, "coverage", "coverage", 12);
        fs::create_dir_all(root.join("notes")).unwrap();
        fs::write(root.join("README"), b"not a campaign").unwrap();
        let expanded = collect_campaigns(&[root]).unwrap();
        assert_eq!(
            expanded
                .iter()
                .map(|campaign| campaign.name.as_str())
                .collect::<Vec<_>>(),
            ["coverage", "unified"]
        );

        let empty = directory.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(collect_campaigns(&[empty]).is_err());

        let file = directory.path().join("plan.yaml");
        fs::write(&file, b"services: {}").unwrap();
        assert!(collect_campaigns(&[file]).is_err());
    }

    #[test]
    fn registry_index_serves_named_bundles_and_refuses_bad_entries() {
        let directory = tempfile::tempdir().unwrap();
        write_named_bundle(directory.path(), "unified", "unified", 12);
        write_exploration_bundle(directory.path(), "exploration");

        let registry = directory.path().join("registry.json");
        fs::write(
            &registry,
            r#"{"format":"theseus-serve-registry-v1","campaigns":[{"name":"unified","directory":"unified"},{"name":"tree","directory":"exploration"}]}"#,
        )
        .unwrap();
        let served = collect_registry(&registry).unwrap();
        assert_eq!(served[0].name, "unified");
        assert_eq!(served[0].kind, "campaign");
        assert_eq!(served[1].name, "tree");
        assert_eq!(served[1].kind, "exploration");

        let page = index_page(&served);
        let page = String::from_utf8(page).unwrap();
        assert!(
            page.contains("campaign · <a href=\"/unified/report\""),
            "{page}"
        );
        assert!(
            page.contains("exploration · <a href=\"/tree/report\""),
            "{page}"
        );
        assert!(page.contains("/unified/plan"), "{page}");
        assert!(!page.contains("/tree/plan"), "{page}");

        let duplicate = directory.path().join("duplicate.json");
        fs::write(
            &duplicate,
            r#"{"format":"theseus-serve-registry-v1","campaigns":[{"name":"unified","directory":"unified"},{"name":"unified","directory":"exploration"}]}"#,
        )
        .unwrap();
        let error = collect_registry(&duplicate).unwrap_err();
        assert!(
            error.contains("registry entry \"unified\" duplicates a name"),
            "{error}"
        );

        let missing = directory.path().join("missing.json");
        fs::write(
            &missing,
            r#"{"format":"theseus-serve-registry-v1","campaigns":[{"name":"gone","directory":"nowhere"}]}"#,
        )
        .unwrap();
        let error = collect_registry(&missing).unwrap_err();
        assert!(
            error.contains("registry entry \"gone\": nowhere"),
            "{error}"
        );

        let unversioned = directory.path().join("unversioned.json");
        fs::write(
            &unversioned,
            r#"{"format":"something-else","campaigns":[]}"#,
        )
        .unwrap();
        assert!(collect_registry(&unversioned).is_err());
    }
}
