// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Offline, single-file inspection reports for Theseus result directories.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum ReportError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    Invalid(String),
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for ReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "cannot parse {}: {source}", path.display())
            }
            Self::Invalid(reason) => formatter.write_str(reason),
            Self::Write { path, source } => {
                write!(formatter, "cannot write {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ReportError {}

#[derive(Deserialize)]
struct ResultRecord {
    format: String,
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    checks: Vec<Check>,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    minimization: Option<Minimization>,
    #[serde(default)]
    replay_verification: Option<ReplayVerification>,
}

#[derive(Clone, Deserialize, Serialize)]
struct ReplayVerification {
    status: String,
    detail: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Minimization {
    original_events_hex: Vec<String>,
    minimized_events_hex: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignMinimization {
    property: String,
    #[serde(default)]
    original_operations: Vec<String>,
    #[serde(default)]
    minimized_operations: Vec<String>,
    #[serde(default)]
    original_faults: Vec<String>,
    #[serde(default)]
    minimized_faults: Vec<String>,
    #[serde(default)]
    operation_attempts: usize,
    #[serde(default)]
    fault_attempts: usize,
}

#[derive(Clone, Deserialize, Serialize)]
struct Check {
    name: String,
    #[serde(default)]
    kind: String,
    status: String,
    detail: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Node {
    search_index: usize,
    id: u64,
    parent: Option<u64>,
    depth: u32,
    seed: u64,
    seed_path: Vec<u64>,
    entropy_probe_hex: String,
    markers_hex: String,
    dirty_pages: Option<u64>,
    #[serde(default)]
    serial_log: Option<String>,
}

#[derive(Deserialize)]
struct TopologyPlan {
    format: String,
    #[serde(default)]
    campaign: Option<CampaignPlan>,
}

#[derive(Deserialize)]
struct CampaignPlan {
    #[serde(default)]
    operations: Vec<CampaignOperation>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignOperation {
    name: String,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
    #[serde(default)]
    requires_markers: Vec<String>,
    #[serde(default)]
    excludes_markers: Vec<String>,
    #[serde(default)]
    max_uses: Option<u8>,
}

#[derive(Deserialize)]
struct ServiceResult {
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    checks: Vec<Check>,
    #[serde(default)]
    faults: Vec<Fault>,
}

#[derive(Deserialize)]
struct CampaignResult {
    format: String,
    status: String,
    driver: String,
    #[serde(default)]
    checkpoint_nodes: usize,
    #[serde(default)]
    checkpoint_reuses: usize,
    #[serde(default)]
    generated_candidates: usize,
    #[serde(default)]
    marker_guard_rejections: usize,
    #[serde(default)]
    unique_topology_states: usize,
    #[serde(default)]
    replay_verification: Option<ReplayVerification>,
    #[serde(default)]
    runs: Vec<CampaignRun>,
    #[serde(default)]
    properties: Vec<CampaignProperty>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignRun {
    index: usize,
    operations: Vec<String>,
    #[serde(default)]
    fault: Option<String>,
    #[serde(default)]
    faults: Vec<String>,
    #[serde(default)]
    actions: Vec<CampaignAction>,
    #[serde(default)]
    selection: String,
    #[serde(default)]
    program_counters: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    state_novel: bool,
    status: String,
    #[serde(default)]
    novelty: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignAction {
    kind: String,
    target: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct CampaignProperty {
    name: String,
    kind: String,
    status: String,
    detail: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Fault {
    round: u64,
    kind: String,
    detail: String,
}

#[derive(Serialize)]
struct ReportModel {
    title: String,
    kind: String,
    status: String,
    error: Option<String>,
    command_label: String,
    command: String,
    path_command: Option<String>,
    minimize_path_command: Option<String>,
    snapshot_path_command: Option<String>,
    checks: Vec<Check>,
    faults: Vec<Fault>,
    logs: Vec<Log>,
    nodes: Vec<Node>,
    coverage: Option<Coverage>,
    minimization: Option<Minimization>,
    campaign_minimization: Option<CampaignMinimization>,
    replay_verification: Option<ReplayVerification>,
    campaign_runs: Vec<CampaignRun>,
    campaign_operations: Vec<CampaignOperation>,
}

#[derive(Serialize)]
struct Log {
    label: String,
    text: String,
}

#[derive(Serialize)]
struct Coverage {
    label: String,
    summary: String,
}

/// Render `input` into a standalone `index.html` under `output`.
///
/// The input is a completed or failed single-timeline replay, topology replay,
/// or exploration directory. All report data comes from files inside that
/// directory; symlinks outside it are rejected.
pub fn report(input: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<PathBuf, ReportError> {
    let root = fs::canonicalize(input.as_ref()).map_err(|source| ReportError::Read {
        path: input.as_ref().to_path_buf(),
        source,
    })?;
    if !root.is_dir() {
        return Err(ReportError::Invalid(format!(
            "report input is not a directory: {}",
            root.display()
        )));
    }
    let output = output.as_ref();
    if output.exists() {
        return Err(ReportError::Invalid(format!(
            "report output already exists: {}",
            output.display()
        )));
    }
    let model = load_model(&root)?;
    fs::create_dir_all(output).map_err(|source| ReportError::Write {
        path: output.to_path_buf(),
        source,
    })?;
    let index = output.join("index.html");
    fs::write(&index, render(&model)?).map_err(|source| ReportError::Write {
        path: index.clone(),
        source,
    })?;
    Ok(index)
}

fn load_model(root: &Path) -> Result<ReportModel, ReportError> {
    let result_path = root.join("result.json");
    if result_path.is_file() {
        let result: ResultRecord = read_json(root, Path::new("result.json"))?;
        return match result.format.as_str() {
            "theseus-result-v1" => single_timeline(root, result),
            "theseus-exploration-result-v1" => exploration(root, result),
            format => Err(ReportError::Invalid(format!(
                "unsupported Theseus result format {format:?}"
            ))),
        };
    }
    topology(root)
}

fn single_timeline(root: &Path, result: ResultRecord) -> Result<ReportModel, ReportError> {
    Ok(ReportModel {
        title: "Timeline replay".to_owned(),
        kind: "one deterministic timeline".to_owned(),
        status: result.status,
        error: result.error,
        command_label: "Replay this locked bundle".to_owned(),
        command: format!("theseus replay {}", shell_quote(root)),
        path_command: None,
        minimize_path_command: None,
        snapshot_path_command: None,
        checks: result.checks,
        faults: Vec::new(),
        logs: maybe_log(root, Path::new("serial.log"), "Serial log")?
            .into_iter()
            .collect(),
        nodes: Vec::new(),
        coverage: None,
        minimization: None,
        campaign_minimization: None,
        replay_verification: result.replay_verification,
        campaign_runs: Vec::new(),
        campaign_operations: Vec::new(),
    })
}

fn exploration(root: &Path, mut result: ResultRecord) -> Result<ReportModel, ReportError> {
    result.nodes.sort_by_key(|node| node.search_index);
    let _: serde_json::Value = read_json(root, Path::new("explore-plan.json"))?;
    let populated = result
        .nodes
        .iter()
        .filter_map(|node| node.dirty_pages)
        .collect::<Vec<_>>();
    let coverage = if populated.is_empty() {
        None
    } else {
        let unique = populated
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        Some(Coverage {
            label: "Dirty-page footprint (coverage proxy)".to_owned(),
            summary: format!(
                "{} captured nodes; {} distinct dirty-page counts; range {}–{} pages",
                populated.len(),
                unique.len(),
                populated.iter().min().unwrap(),
                populated.iter().max().unwrap()
            ),
        })
    };
    let logs = result
        .nodes
        .iter()
        .filter_map(|node| {
            node.serial_log.as_deref().map(|serial_log| {
                maybe_log(
                    root,
                    Path::new(serial_log),
                    &format!("Timeline #{} serial log", node.search_index),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(ReportModel {
        title: "Exploration".to_owned(),
        kind: "deterministic timeline search".to_owned(),
        status: result.status,
        error: result.error,
        command_label: "Replay this locked exploration".to_owned(),
        command: format!(
            "theseus explore --replay {} --output exploration-rerun",
            shell_quote(root)
        ),
        path_command: Some(format!(
            "theseus explore --replay {} --seed-path ",
            shell_quote(root)
        )),
        minimize_path_command: Some(format!(
            "theseus explore --minimize {} --seed-path ",
            shell_quote(root)
        )),
        snapshot_path_command: Some(format!(
            "theseus explore --snapshot {} --seed-path ",
            shell_quote(root)
        )),
        checks: result.checks,
        faults: Vec::new(),
        logs,
        nodes: result.nodes,
        coverage,
        minimization: result.minimization,
        campaign_minimization: None,
        replay_verification: result.replay_verification,
        campaign_runs: Vec::new(),
        campaign_operations: Vec::new(),
    })
}

fn topology(root: &Path) -> Result<ReportModel, ReportError> {
    let plan: TopologyPlan = read_json(root, Path::new("replay-plan.json"))?;
    if plan.format != "theseus-compose-plan-v1" {
        return Err(ReportError::Invalid(
            "directory has neither a Theseus result nor a topology replay plan".to_owned(),
        ));
    }
    if root.join("campaign-result.json").is_file() {
        return campaign(root);
    }
    let campaign_minimization = root
        .join("minimization.json")
        .is_file()
        .then(|| read_json(root, Path::new("minimization.json")))
        .transpose()?;
    let services = root.join("services");
    let entries = fs::read_dir(&services).map_err(|source| ReportError::Read {
        path: services.clone(),
        source,
    })?;
    let mut results = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|source| ReportError::Read {
            path: services.clone(),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = PathBuf::from("services").join(&name).join("result.json");
        if local_path(root, &relative).is_ok_and(|path| path.is_file()) {
            results.insert(name, read_json::<ServiceResult>(root, &relative)?);
        }
    }
    if results.is_empty() {
        return Err(ReportError::Invalid(format!(
            "topology replay has no service results: {}",
            services.display()
        )));
    }
    let mut checks = Vec::new();
    let mut faults = Vec::new();
    let mut logs = Vec::new();
    let mut failed = false;
    let mut errors = Vec::new();
    for (service, result) in results {
        failed |= result.status != "passed";
        if let Some(error) = result.error {
            errors.push(format!("{service}: {error}"));
        }
        checks.extend(result.checks.into_iter().map(|mut check| {
            check.name = format!("{service}: {}", check.name);
            check
        }));
        faults.extend(result.faults.into_iter().map(|mut fault| {
            fault.detail = format!("{service}: {}", fault.detail);
            fault
        }));
        let service_dir = PathBuf::from("services").join(&service);
        logs.extend(service_logs(root, &service_dir, &service)?);
    }
    Ok(ReportModel {
        title: "Topology replay".to_owned(),
        kind: "deterministic service topology".to_owned(),
        status: if failed { "failed" } else { "passed" }.to_owned(),
        error: (!errors.is_empty()).then(|| errors.join("\n")),
        command_label: "Replay this locked topology".to_owned(),
        command: format!(
            "theseus compose replay {} --output topology-rerun",
            shell_quote(root)
        ),
        path_command: None,
        minimize_path_command: None,
        snapshot_path_command: None,
        checks,
        faults,
        logs,
        nodes: Vec::new(),
        coverage: None,
        minimization: None,
        campaign_minimization,
        replay_verification: None,
        campaign_runs: Vec::new(),
        campaign_operations: Vec::new(),
    })
}

fn campaign(root: &Path) -> Result<ReportModel, ReportError> {
    let result: CampaignResult = read_json(root, Path::new("campaign-result.json"))?;
    let plan: TopologyPlan = read_json(root, Path::new("replay-plan.json"))?;
    if result.format != "theseus-compose-campaign-result-v1" {
        return Err(ReportError::Invalid(format!(
            "unsupported campaign result format {:?}",
            result.format
        )));
    }
    let checks = result
        .properties
        .iter()
        .cloned()
        .map(|property| Check {
            name: property.name,
            kind: property.kind,
            status: property.status,
            detail: property.detail,
        })
        .collect();
    Ok(ReportModel {
        title: "Autonomous Compose campaign".to_owned(),
        kind: format!("deterministic topology search driven by {}", result.driver),
        status: result.status,
        error: None,
        command_label: "Replay this locked campaign".to_owned(),
        command: format!(
            "theseus compose replay {} --output campaign-rerun",
            shell_quote(root)
        ),
        path_command: None,
        minimize_path_command: None,
        snapshot_path_command: None,
        checks,
        faults: Vec::new(),
        logs: Vec::new(),
        nodes: Vec::new(),
        coverage: Some(Coverage {
            label: "Campaign corpus".to_owned(),
            summary: format!(
                "{} of {} deterministic candidates selected by marker and topology-state coverage; {} marker-guard leaves skipped; {} unique topology states; {} reusable checkpoint nodes, {} prefix reuses",
                result.runs.len(),
                result.generated_candidates,
                result.marker_guard_rejections,
                result.unique_topology_states,
                result.checkpoint_nodes,
                result.checkpoint_reuses,
            ),
        }),
        minimization: None,
        campaign_minimization: None,
        replay_verification: result.replay_verification,
        campaign_runs: result.runs,
        campaign_operations: plan
            .campaign
            .map(|campaign| campaign.operations)
            .unwrap_or_default(),
    })
}

fn service_logs(root: &Path, directory: &Path, service: &str) -> Result<Vec<Log>, ReportError> {
    let path = local_path(root, directory)?;
    let entries = fs::read_dir(&path).map_err(|source| ReportError::Read {
        path: path.clone(),
        source,
    })?;
    let mut names = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name == "serial.log" || (name.starts_with("serial-") && name.ends_with(".log"))
        })
        .collect::<Vec<_>>();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let relative = directory.join(&name);
            Ok(Log {
                label: format!("{service}: {name}"),
                text: read_text(root, &relative)?,
            })
        })
        .collect()
}

fn maybe_log(root: &Path, relative: &Path, label: &str) -> Result<Option<Log>, ReportError> {
    match local_path(root, relative) {
        Ok(path) if path.is_file() => Ok(Some(Log {
            label: label.to_owned(),
            text: read_text(root, relative)?,
        })),
        Ok(_) => Ok(None),
        Err(ReportError::Read { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn read_json<T: for<'a> Deserialize<'a>>(root: &Path, relative: &Path) -> Result<T, ReportError> {
    let path = local_path(root, relative)?;
    let bytes = fs::read(&path).map_err(|source| ReportError::Read {
        path: path.clone(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| ReportError::Parse { path, source })
}

fn read_text(root: &Path, relative: &Path) -> Result<String, ReportError> {
    let path = local_path(root, relative)?;
    let bytes = fs::read(&path).map_err(|source| ReportError::Read {
        path: path.clone(),
        source,
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn local_path(root: &Path, relative: &Path) -> Result<PathBuf, ReportError> {
    if relative.is_absolute() || relative.components().any(|part| part.as_os_str() == "..") {
        return Err(ReportError::Invalid(format!(
            "report path escapes its result directory: {}",
            relative.display()
        )));
    }
    let requested = root.join(relative);
    let path = fs::canonicalize(&requested).map_err(|source| ReportError::Read {
        path: requested,
        source,
    })?;
    if !path.starts_with(root) {
        return Err(ReportError::Invalid(format!(
            "report path escapes its result directory: {}",
            relative.display()
        )));
    }
    Ok(path)
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

fn render(model: &ReportModel) -> Result<String, ReportError> {
    let data = serde_json::to_string(model)
        .map_err(|error| ReportError::Invalid(format!("cannot encode report data: {error}")))?
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    Ok(format!(
        r##"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Theseus report</title>
<style>
:root {{ color-scheme: light dark; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }}
body {{ max-width: 1080px; margin: 2rem auto; padding: 0 1rem; line-height: 1.45; }}
h1 {{ margin-bottom: 0; }} .muted {{ color: #667085; }} .status {{ font-weight: bold; }}
.passed {{ color: #157347; }} .failed {{ color: #b42318; }} section {{ border-top: 1px solid #98a2b3; margin-top: 1.5rem; }}
pre {{ overflow: auto; padding: 1rem; background: #101828; color: #f2f4f7; }}
table {{ width: 100%; border-collapse: collapse; }} th, td {{ text-align: left; vertical-align: top; padding: .45rem; border-bottom: 1px solid #d0d5dd; }}
.node {{ border-left: 2px solid #98a2b3; margin: .45rem 0; padding-left: .7rem; }} .node p {{ margin: .2rem 0; }}
</style><body><main id="report"></main>
<script id="report-data" type="application/json">{data}</script>
<script>
const m=JSON.parse(document.querySelector('#report-data').textContent), app=document.querySelector('#report');
const el=(tag,text)=>{{const x=document.createElement(tag);if(text!==undefined)x.textContent=text;return x}};
const section=(title)=>{{const s=el('section'),h=el('h2',title);s.append(h);app.append(s);return s}};
const table=(rows,heads)=>{{const t=el('table'),tr=el('tr');heads.forEach(h=>tr.append(el('th',h)));t.append(tr);rows.forEach(row=>{{const r=el('tr');row.forEach(value=>r.append(el('td',value)));t.append(r)}});return t}};
app.append(el('h1',m.title)); app.append(el('p',m.kind));
const status=el('p','Status: '+m.status);status.className='status '+m.status;app.append(status);
if(m.error){{const e=section('Error');e.append(el('pre',m.error));}}
const replay=section(m.command_label);replay.append(el('pre',m.command));
if(m.nodes.length){{const s=section('Timeline tree');m.nodes.forEach(n=>{{const d=el('div');d.className='node';d.style.marginLeft=(n.depth*1.25)+'rem';d.append(el('strong','#'+n.search_index+' · node '+n.id+' · seed '+n.seed));d.append(el('p','parent: '+(n.parent===null?'root':n.parent)+' · seed path: '+n.seed_path.join(' → ')));if(m.path_command){{d.append(el('code',m.path_command+n.seed_path.join(',')));}}if(m.snapshot_path_command){{d.append(el('p','Export this paused timeline:'));d.append(el('code',m.snapshot_path_command+n.seed_path.join(',')));}}if(m.minimize_path_command&&m.status==='failed'){{d.append(el('p','Minimize this failing path:'));d.append(el('code',m.minimize_path_command+n.seed_path.join(',')));}}d.append(el('p','markers: '+(n.markers_hex||'none')+' · dirty pages: '+(n.dirty_pages===null?'not captured':n.dirty_pages)));if(n.serial_log){{d.append(el('p','serial log: '+n.serial_log));}}d.append(el('p','entropy probe: '+n.entropy_probe_hex));s.append(d)}});}}
if(m.coverage){{const s=section(m.coverage.label);s.append(el('p',m.coverage.summary));}}
if(m.campaign_operations.length){{const s=section('Operation model');s.append(table(m.campaign_operations.map(o=>[o.name,o.requires.join(' + ')||'none',o.excludes.join(' + ')||'none',o.requires_markers.join(' + ')||'none',o.excludes_markers.join(' + ')||'none',o.max_uses===null?'unbounded':String(o.max_uses)]),['Operation','Requires earlier','Excludes earlier','Requires observed marker','Excludes observed marker','Maximum uses']));}}
if(m.campaign_runs.length){{const s=section('Generated timelines');s.append(table(m.campaign_runs.map(r=>[String(r.index),r.operations.join(' → ')||'none',(r.faults.length?r.faults:(r.fault?[r.fault]:[])).join(' + ')||'none',r.selection||'canonical breadth-first seed',r.state_novel?'new':'seen',Object.entries(r.program_counters).map(([service,pcs])=>service+': '+pcs.join(' ')).join(' · ')||'none',r.actions.map(a=>a.kind+' '+a.target).join(' · ')||'none',r.status,r.novelty.join(' ')||'none']),['Run','Operations','Candidates','Selection','Topology state','Paused PCs','Applied actions','Status','New markers']));}}
if(m.minimization){{const s=section('Event minimization');s.append(table([[m.minimization.original_events_hex.join(' ')||'none',m.minimization.minimized_events_hex.join(' ')||'none']],['Original events','1-minimal events']));}}
if(m.campaign_minimization){{const x=m.campaign_minimization,s=section('Campaign minimization');s.append(table([[x.property,x.original_operations.join(' → ')||'none',x.minimized_operations.join(' → ')||'none',x.original_faults.join(' + ')||'none',x.minimized_faults.join(' + ')||'none',String(x.operation_attempts),String(x.fault_attempts)]],['Property','Original operations','1-minimal operations','Original faults','1-minimal faults','Operation replays','Fault replays']));}}
if(m.replay_verification){{const s=section('Replay verification');s.append(table([[m.replay_verification.status,m.replay_verification.detail]],['Status','Detail']));}}
if(m.checks.length){{const s=section('Checks');s.append(table(m.checks.map(c=>[c.name,c.kind,c.status,c.detail]),['Name','Kind','Status','Detail']));}}
if(m.faults.length){{const s=section('Applied faults');s.append(table(m.faults.map(f=>[String(f.round),f.kind,f.detail]),['Round','Kind','Detail']));}}
if(m.logs.length){{const s=section('Logs');m.logs.forEach(log=>{{s.append(el('h3',log.label));s.append(el('pre',log.text));}});}}
</script></body></html>"##
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_json(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn renders_a_safe_single_timeline_report() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("result.json"),
            r#"{"format":"theseus-result-v1","status":"failed","error":null,"checks":[{"name":"no-panic","kind":"serial_not_contains","status":"passed","detail":"ok"}]}"#,
        );
        fs::write(
            directory.path().join("serial.log"),
            b"<img src=x onerror=alert(1)>",
        )
        .unwrap();
        let output = directory.path().join("report");
        let index = report(directory.path(), &output).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Timeline replay"));
        assert!(html.contains("theseus replay"));
        assert!(html.contains("\\u003cimg"));
        assert!(!html.contains("<img src=x"));
    }

    #[test]
    fn renders_topology_faults_and_service_logs() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","compose":"/tmp/compose.yaml"}"#,
        );
        let service = directory.path().join("services/api");
        fs::create_dir_all(&service).unwrap();
        write_json(
            &service.join("result.json"),
            r#"{"status":"passed","checks":[{"name":"guest_exit","status":"passed","detail":"ok"}],"faults":[{"round":2,"kind":"restart","detail":"restarted"}]}"#,
        );
        write_json(
            &directory.path().join("minimization.json"),
            r#"{"property":"consistent_read","original_operations":["write","retry","read"],"minimized_operations":["write","read"],"original_faults":["backplane:partition@write","backplane:heal@read"],"minimized_faults":["backplane:partition@write"],"operation_attempts":4,"fault_attempts":2}"#,
        );
        fs::write(service.join("serial.log"), b"ready\n").unwrap();
        let index = report(directory.path(), directory.path().join("report")).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Topology replay"));
        assert!(html.contains("restart"));
        assert!(html.contains("api: serial.log"));
        assert!(html.contains("Campaign minimization"));
        assert!(html.contains("1-minimal faults"));
        assert!(html.contains("backplane:partition@write"));
        assert!(html.contains("Operation replays"));
    }

    #[test]
    fn renders_an_autonomous_compose_campaign() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("replay-plan.json"),
            r#"{"format":"theseus-compose-plan-v1","campaign":{"operations":[{"name":"write","max_uses":1},{"name":"close","requires":["write"],"requires_markers":["written"],"max_uses":1},{"name":"read","requires":["write"],"excludes":["close"],"excludes_markers":["closed"]}]}}"#,
        );
        write_json(
            &directory.path().join("campaign-result.json"),
            r#"{"format":"theseus-compose-campaign-result-v1","status":"failed","driver":"api","checkpoint_nodes":4,"checkpoint_reuses":7,"generated_candidates":12,"marker_guard_rejections":2,"unique_topology_states":3,"replay_verification":{"status":"passed","detail":"1 recorded campaign timelines reproduced"},"runs":[{"index":0,"operations":["write","read"],"faults":["backplane:partition@write","backplane:heal@read"],"selection":"extends 1-operation prefix with 2 new marker(s) and new topology state","program_counters":{"api":["0x8000"]},"state_novel":true,"actions":[{"kind":"partition","target":"network:backplane"}],"status":"failed","novelty":["42","a1"]}],"properties":[{"name":"consistent_read","kind":"always","status":"failed","detail":"0 of 1 retained timelines contained \"pass\""}]}"#,
        );
        let index = report(directory.path(), directory.path().join("report")).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Autonomous Compose campaign"));
        assert!(html.contains("Generated timelines"));
        assert!(html.contains("backplane:partition@write"));
        assert!(html.contains("backplane:heal@read"));
        assert!(html.contains("Candidates"));
        assert!(html.contains("Applied actions"));
        assert!(html.contains("network:backplane"));
        assert!(html.contains("consistent_read"));
        assert!(html.contains("4 reusable checkpoint nodes, 7 prefix reuses"));
        assert!(html.contains(
            "1 of 12 deterministic candidates selected by marker and topology-state coverage"
        ));
        assert!(html.contains("2 marker-guard leaves skipped"));
        assert!(html.contains("3 unique topology states"));
        assert!(
            html.contains("extends 1-operation prefix with 2 new marker(s) and new topology state")
        );
        assert!(html.contains("Topology state"));
        assert!(html.contains("Paused PCs"));
        assert!(html.contains("0x8000"));
        assert!(html.contains("Operation model"));
        assert!(html.contains("Requires earlier"));
        assert!(html.contains("Excludes earlier"));
        assert!(html.contains("Requires observed marker"));
        assert!(html.contains("Excludes observed marker"));
        assert!(html.contains("Maximum uses"));
        assert!(html.contains("unbounded"));
        assert!(html.contains("Replay verification"));
        assert!(html.contains("1 recorded campaign timelines reproduced"));
    }

    #[test]
    fn renders_exploration_tree_and_coverage_proxy() {
        let directory = tempfile::tempdir().unwrap();
        write_json(
            &directory.path().join("explore-plan.json"),
            r#"{"manifest":"/tmp/theseus.toml"}"#,
        );
        write_json(
            &directory.path().join("result.json"),
            r#"{"format":"theseus-exploration-result-v1","status":"failed","error":null,"checks":[{"name":"every timeline completed","kind":"marker_seen","status":"failed","detail":"missing ff"}],"nodes":[{"search_index":0,"id":0,"parent":null,"depth":0,"seed":1,"seed_path":[1],"entropy_probe_hex":"aa","markers_hex":"42","dirty_pages":3,"serial_log":"serial/1.log"},{"search_index":1,"id":1,"parent":0,"depth":1,"seed":2,"seed_path":[1,2],"entropy_probe_hex":"bb","markers_hex":"43","dirty_pages":5,"serial_log":"serial/2.log"}],"minimization":{"original_events_hex":["01","02","03"],"minimized_events_hex":["02"]}}"#,
        );
        fs::create_dir(directory.path().join("serial")).unwrap();
        fs::write(directory.path().join("serial/1.log"), b"root ready\n").unwrap();
        fs::write(directory.path().join("serial/2.log"), b"child ready\n").unwrap();
        let index = report(directory.path(), directory.path().join("report")).unwrap();
        let html = fs::read_to_string(index).unwrap();
        assert!(html.contains("Timeline tree"));
        assert!(html.contains("Dirty-page footprint"));
        assert!(html.contains("every timeline completed"));
        assert!(html.contains("exploration-rerun"));
        assert!(html.contains("--seed-path"));
        assert!(html.contains("--minimize"));
        assert!(html.contains("--snapshot"));
        assert!(html.contains("Event minimization"));
        assert!(html.contains("Timeline #1 serial log"));
        assert!(html.contains("child ready"));
    }
}
