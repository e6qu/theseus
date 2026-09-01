// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Linux+KVM executor for a normalized Theseus exploration plan.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use event_manager::EventManager;
use serde::Serialize;
use sha2::{Digest, Sha256};
use theseus_cli::{
    ArtifactPlan, CheckKind, CheckPlan, ExplorePlan, Novelty, ReplayFingerprint, ReplayTreeNode,
    RunPlan,
};
use theseus_orchestrator::orchestrator::explorer::{Explorer, ExplorerConfig, NoveltyStrategy};
use theseus_orchestrator::orchestrator::tree::NodeId;
use vmm::builder::build_microvm_for_boot;
use vmm::resources::VmResources;
use vmm::seccomp::get_empty_filters;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::entropy::EntropyDeviceConfig;
use vmm::vmm_config::instance_info::InstanceInfo;
use vmm::vmm_config::machine_config::{MachineConfigUpdate, VirtualTimeConfig};

const USAGE: &str = "Usage: theseus-explorer --plan explore-plan.json --output exploration-dir [--minimize|--snapshot]";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Explore,
    Minimize,
    Snapshot,
}

#[derive(Serialize)]
struct ResultRecord {
    format: &'static str,
    status: &'static str,
    error: Option<String>,
    checks: Vec<CheckResult>,
    nodes: Vec<NodeRecord>,
    minimization: Option<Minimization>,
    replay_verification: Option<ReplayVerification>,
}

#[derive(Serialize)]
struct Minimization {
    original_events_hex: Vec<String>,
    minimized_events_hex: Vec<String>,
}

#[derive(Serialize)]
struct CheckResult {
    name: String,
    kind: &'static str,
    status: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct NodeRecord {
    search_index: usize,
    id: NodeId,
    parent: Option<NodeId>,
    depth: u32,
    seed: u64,
    seed_path: Vec<u64>,
    entropy_probe_hex: String,
    markers_hex: String,
    dirty_pages: Option<u64>,
    serial_log: String,
    #[serde(skip)]
    serial: Vec<u8>,
}

#[derive(Serialize)]
struct SnapshotRecord {
    format: &'static str,
    state: &'static str,
    memory: &'static str,
    seed_path: Vec<u64>,
    entropy_probe_hex: String,
    markers_hex: String,
    dirty_pages: Option<u64>,
}

#[derive(Serialize)]
struct ReplayVerification {
    status: &'static str,
    detail: String,
}

struct Execution {
    nodes: Vec<NodeRecord>,
    checks: Vec<CheckResult>,
    replay_verification: Option<ReplayVerification>,
}

fn main() -> std::process::ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("theseus-explorer: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let (plan_path, output, mode) = match args.as_slice() {
        [flag_plan, plan_path, flag_output, output]
            if flag_plan == "--plan" && flag_output == "--output" =>
        {
            (plan_path, output, Mode::Explore)
        }
        [flag_plan, plan_path, flag_output, output, flag_minimize]
            if flag_plan == "--plan"
                && flag_output == "--output"
                && flag_minimize == "--minimize" =>
        {
            (plan_path, output, Mode::Minimize)
        }
        [flag_plan, plan_path, flag_output, output, flag_snapshot]
            if flag_plan == "--plan"
                && flag_output == "--output"
                && flag_snapshot == "--snapshot" =>
        {
            (plan_path, output, Mode::Snapshot)
        }
        _ => return Err(USAGE.to_owned()),
    };
    let plan: RunPlan = serde_json::from_slice(
        &fs::read(plan_path).map_err(|error| format!("cannot read {plan_path}: {error}"))?,
    )
    .map_err(|error| format!("cannot parse exploration plan: {error}"))?;
    let output = PathBuf::from(output);
    if output.exists() {
        return Err(format!(
            "exploration output already exists: {}",
            output.display()
        ));
    }
    fs::create_dir_all(output.join("artifacts")).map_err(|error| error.to_string())?;
    let serial_logs = output.join("serial");
    fs::create_dir(&serial_logs).map_err(|error| error.to_string())?;
    let mut plan = lock_plan(plan, &output)?;
    write_plan(&output, &plan)?;

    let original_events_hex = plan
        .explore
        .as_ref()
        .map(|explore| explore.events_hex.clone())
        .unwrap_or_default();
    let result = if mode == Mode::Minimize {
        minimize_events(&mut plan, &serial_logs)
    } else {
        execute(
            &plan,
            (mode == Mode::Snapshot).then_some(&output),
            &serial_logs,
        )
    };
    write_plan(&output, &plan)?;
    match result {
        Ok(execution) => {
            let failed = execution
                .checks
                .iter()
                .filter(|check| check.status == "failed")
                .map(|check| check.name.clone())
                .collect::<Vec<_>>();
            let replay_failed = execution
                .replay_verification
                .as_ref()
                .is_some_and(|verification| verification.status == "failed");
            write_result(
                &output,
                if failed.is_empty() && !replay_failed {
                    "passed"
                } else {
                    "failed"
                },
                None,
                execution.checks,
                execution.nodes,
                (mode == Mode::Minimize).then(|| Minimization {
                    original_events_hex,
                    minimized_events_hex: plan
                        .explore
                        .as_ref()
                        .expect("exploration plan was executed")
                        .events_hex
                        .clone(),
                }),
                execution.replay_verification,
            )?;
            if mode == Mode::Minimize
                || (!replay_failed && (mode == Mode::Snapshot || failed.is_empty()))
            {
                Ok(())
            } else {
                Err(if replay_failed {
                    "replay fingerprint changed".to_owned()
                } else {
                    format!("checks failed: {}", failed.join(", "))
                })
            }
        }
        Err(error) => {
            write_result(
                &output,
                "failed",
                Some(error.clone()),
                Vec::new(),
                Vec::new(),
                None,
                None,
            )?;
            Err(error)
        }
    }
}

/// Greedily remove events until removing any remaining event changes the set
/// of failed named properties. This is deterministic and yields a 1-minimal
/// event sequence, not a globally minimal one.
fn minimize_events(plan: &mut RunPlan, serial_logs: &Path) -> Result<Execution, String> {
    let baseline = execute(plan, None, serial_logs)?;
    let expected = failed_names(&baseline);
    if expected.is_empty() {
        return Err("minimization requires a property-failing seed path".to_owned());
    }
    let original = plan
        .explore
        .as_ref()
        .expect("exploration plan was executed")
        .events_hex
        .clone();
    let minimized = reduce_events(original, |candidate| {
        plan.explore
            .as_mut()
            .expect("exploration plan was executed")
            .events_hex = candidate.to_vec();
        execute(plan, None, serial_logs).is_ok_and(|execution| failed_names(&execution) == expected)
    });
    plan.explore
        .as_mut()
        .expect("exploration plan was executed")
        .events_hex = minimized;
    execute(plan, None, serial_logs)
}

fn failed_names(execution: &Execution) -> Vec<String> {
    execution
        .checks
        .iter()
        .filter(|check| check.status == "failed")
        .map(|check| check.name.clone())
        .collect()
}

fn reduce_events(
    mut events: Vec<String>,
    mut preserves_failure: impl FnMut(&[String]) -> bool,
) -> Vec<String> {
    loop {
        let mut removed = false;
        for index in 0..events.len() {
            let mut candidate = events.clone();
            candidate.remove(index);
            if preserves_failure(&candidate) {
                events = candidate;
                removed = true;
                break;
            }
        }
        if !removed {
            return events;
        }
    }
}

fn execute(
    plan: &RunPlan,
    snapshot_output: Option<&Path>,
    serial_logs: &Path,
) -> Result<Execution, String> {
    let explore = plan
        .explore
        .as_ref()
        .ok_or_else(|| "exploration plan has no [explore] contract".to_owned())?;
    if !explore.rendezvous {
        return Err(
            "exploration requires explore.rendezvous = true; host-time runs are not replayable"
                .to_owned(),
        );
    }
    if snapshot_output.is_some() && explore.replay_seed_path.is_none() {
        return Err("snapshot export requires a targeted replay seed path".to_owned());
    }
    if !plan.events.is_empty()
        || !plan.storage.is_empty()
        || plan.network.loopback
        || plan.network.drop_ppm != 0
        || plan.network.partitioned
    {
        return Err(
            "exploration currently accepts only the headless SDK control-channel VM".to_owned(),
        );
    }
    validate_checks(&plan.checks)?;
    let resources = resources_from_plan(plan, Some(serial_log_path(serial_logs, plan.run.seed)))?;
    let config = explorer_config(explore, Some(serial_logs.to_path_buf()))?;
    let mut event_manager = EventManager::new().map_err(|error| error.to_string())?;
    let filters = get_empty_filters();
    let explorer = if let Some(seed_path) = &explore.replay_seed_path {
        if seed_path.first() != Some(&plan.run.seed) {
            return Err(format!(
                "replay seed path starts at {:?}, expected {}",
                seed_path.first(),
                plan.run.seed
            ));
        }
        Explorer::explore_path(
            seed_path,
            &config,
            &InstanceInfo::default(),
            &filters,
            &mut event_manager,
            |info, manager, filters| {
                build_microvm_for_boot(info, &resources, manager, filters)
                    .map_err(theseus_orchestrator::orchestrator::explorer::ExplorerError::from)
            },
            &VmResources::default,
        )
    } else {
        Explorer::explore(
            plan.run.seed,
            &config,
            &InstanceInfo::default(),
            &filters,
            &mut event_manager,
            |info, manager, filters| {
                build_microvm_for_boot(info, &resources, manager, filters)
                    .map_err(theseus_orchestrator::orchestrator::explorer::ExplorerError::from)
            },
            &VmResources::default,
        )
    }
    .map_err(|error| error.to_string())?;

    let nodes = explorer
        .search_order()
        .iter()
        .copied()
        .into_iter()
        .enumerate()
        .map(|(search_index, id)| {
            let node = explorer.tree.node(id);
            let payload = node.payload.as_ref().expect("captured exploration node");
            NodeRecord {
                search_index,
                id,
                parent: node.parent,
                depth: node.depth,
                seed: node.seed,
                seed_path: explorer.tree.seed_path(id),
                entropy_probe_hex: hex(&payload.entropy_probe),
                markers_hex: hex(&payload.markers),
                dirty_pages: payload.dirty_pages,
                serial_log: format!("serial/{}.log", node.seed),
                serial: payload.serial_log.clone(),
            }
        })
        .collect::<Vec<_>>();
    let checks = evaluate_checks(&plan.checks, &nodes)?;
    let replay_verification = explore
        .replay_expected_tree
        .as_deref()
        .map(|expected| verify_tree(expected, &nodes))
        .or_else(|| {
            explore.replay_expected.as_ref().map(|expected| {
                verify_fingerprint(
                    expected,
                    nodes.last().expect("targeted replay has one node"),
                )
            })
        });
    if replay_verification
        .as_ref()
        .is_none_or(|verification| verification.status == "passed")
    {
        if let Some(output) = snapshot_output {
            export_snapshot(&explorer, output)?;
        }
    }
    Ok(Execution {
        nodes,
        checks,
        replay_verification,
    })
}

fn verify_fingerprint(expected: &ReplayFingerprint, actual: &NodeRecord) -> ReplayVerification {
    let matched = fingerprint_matches(expected, actual);
    ReplayVerification {
        status: if matched { "passed" } else { "failed" },
        detail: if matched {
            "recorded entropy, markers, and dirty-page fingerprints reproduced".to_owned()
        } else {
            "recorded fingerprint differs from this replay".to_owned()
        },
    }
}

fn verify_tree(expected: &[ReplayTreeNode], actual: &[NodeRecord]) -> ReplayVerification {
    if expected.len() != actual.len() {
        return ReplayVerification {
            status: "failed",
            detail: format!(
                "recorded {} timelines, replay captured {}",
                expected.len(),
                actual.len()
            ),
        };
    }
    for (expected, actual) in expected.iter().zip(actual) {
        if expected.seed_path != actual.seed_path {
            return ReplayVerification {
                status: "failed",
                detail: format!(
                    "recorded seed path {:?}, replay captured {:?}",
                    expected.seed_path, actual.seed_path
                ),
            };
        }
        if !fingerprint_matches(&expected.fingerprint, actual) {
            return ReplayVerification {
                status: "failed",
                detail: format!(
                    "recorded fingerprint differs at seed path {:?}",
                    actual.seed_path
                ),
            };
        }
    }
    ReplayVerification {
        status: "passed",
        detail: format!(
            "recorded entropy, markers, and dirty-page fingerprints reproduced for {} timelines",
            actual.len()
        ),
    }
}

fn fingerprint_matches(expected: &ReplayFingerprint, actual: &NodeRecord) -> bool {
    expected.entropy_probe_hex == actual.entropy_probe_hex
        && expected.markers_hex == actual.markers_hex
        && expected.dirty_pages == actual.dirty_pages
        && expected
            .serial_sha256
            .as_ref()
            .is_none_or(|expected| *expected == hex(Sha256::digest(&actual.serial)))
}

fn export_snapshot(explorer: &Explorer, output: &Path) -> Result<(), String> {
    let id = *explorer
        .search_order()
        .last()
        .ok_or_else(|| "snapshot export requires a captured timeline".to_owned())?;
    let node = explorer.tree.node(id);
    let payload = node
        .payload
        .as_ref()
        .ok_or_else(|| "snapshot export target was not captured".to_owned())?;
    payload
        .branch_point
        .export_snapshot(
            &output.join("snapshot.state"),
            &output.join("snapshot.memory"),
        )
        .map_err(|error| error.to_string())?;
    let snapshot = SnapshotRecord {
        format: "theseus-exploration-snapshot-v1",
        state: "snapshot.state",
        memory: "snapshot.memory",
        seed_path: explorer.tree.seed_path(id),
        entropy_probe_hex: hex(&payload.entropy_probe),
        markers_hex: hex(&payload.markers),
        dirty_pages: payload.dirty_pages,
    };
    fs::write(
        output.join("snapshot.json"),
        serde_json::to_vec_pretty(&snapshot).unwrap(),
    )
    .map_err(|error| error.to_string())
}

fn evaluate_checks(checks: &[CheckPlan], nodes: &[NodeRecord]) -> Result<Vec<CheckResult>, String> {
    checks
        .iter()
        .map(|check| {
            let (kind, must_be_present, value) = match check.kind {
                CheckKind::MarkerSeen | CheckKind::MarkerNotSeen => {
                    let marker = marker_byte(&check.value).map_err(|reason| {
                        format!("check {:?} has invalid marker value: {reason}", check.name)
                    })?;
                    (
                        if matches!(check.kind, CheckKind::MarkerSeen) {
                            "marker_seen"
                        } else {
                            "marker_not_seen"
                        },
                        matches!(check.kind, CheckKind::MarkerSeen),
                        format!("marker {marker:02x}"),
                    )
                }
                CheckKind::SerialContains | CheckKind::SerialNotContains => (
                    if matches!(check.kind, CheckKind::SerialContains) {
                        "serial_contains"
                    } else {
                        "serial_not_contains"
                    },
                    matches!(check.kind, CheckKind::SerialContains),
                    format!("serial text {:?}", check.value),
                ),
            };
            let violating = nodes
                .iter()
                .filter(|node| {
                    let present = match check.kind {
                        CheckKind::MarkerSeen | CheckKind::MarkerNotSeen => marker_present(
                            &node.markers_hex,
                            marker_byte(&check.value).expect("validated marker"),
                        ),
                        CheckKind::SerialContains | CheckKind::SerialNotContains => {
                            contains(&node.serial, check.value.as_bytes())
                        }
                    };
                    present != must_be_present
                })
                .collect::<Vec<_>>();
            if violating.is_empty() {
                Ok(CheckResult {
                    name: check.name.clone(),
                    kind,
                    status: "passed",
                    detail: format!(
                        "all {} captured timelines {} {value}",
                        nodes.len(),
                        if must_be_present {
                            "contained"
                        } else {
                            "avoided"
                        }
                    ),
                })
            } else {
                let timelines = violating
                    .iter()
                    .take(3)
                    .map(|node| format!("#{} {:?}", node.search_index, node.seed_path))
                    .collect::<Vec<_>>()
                    .join(", ");
                let extra = violating.len().saturating_sub(3);
                Ok(CheckResult {
                    name: check.name.clone(),
                    kind,
                    status: "failed",
                    detail: format!(
                        "{} {value}: {timelines}{}",
                        if must_be_present {
                            "missing"
                        } else {
                            "present in"
                        },
                        if extra == 0 {
                            String::new()
                        } else {
                            format!(" and {extra} more")
                        }
                    ),
                })
            }
        })
        .collect()
}

fn validate_checks(checks: &[CheckPlan]) -> Result<(), String> {
    for check in checks {
        match check.kind {
            CheckKind::MarkerSeen | CheckKind::MarkerNotSeen => {
                marker_byte(&check.value).map_err(|reason| {
                    format!("check {:?} has invalid marker value: {reason}", check.name)
                })?;
            }
            CheckKind::SerialContains | CheckKind::SerialNotContains => {}
        }
    }
    Ok(())
}

fn marker_byte(value: &str) -> Result<u8, &'static str> {
    if value.len() != 2 {
        return Err("expected one two-digit hexadecimal byte");
    }
    u8::from_str_radix(value, 16).map_err(|_| "expected one two-digit hexadecimal byte")
}

fn marker_present(markers_hex: &str, marker: u8) -> bool {
    markers_hex.as_bytes().chunks_exact(2).any(|hex| {
        u8::from_str_radix(std::str::from_utf8(hex).expect("hex is ASCII"), 16)
            .is_ok_and(|value| value == marker)
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn resources_from_plan(plan: &RunPlan, serial_log: Option<PathBuf>) -> Result<VmResources, String> {
    let mut resources = VmResources::default();
    resources
        .build_boot_source(BootSourceConfig {
            kernel_image_path: plan.guest.kernel.path.clone(),
            initrd_path: Some(plan.guest.initramfs.path.clone()),
            boot_args: Some("console=ttyS0 reboot=k panic=-1".to_owned()),
        })
        .map_err(|error| error.to_string())?;
    resources
        .update_machine_config(&MachineConfigUpdate {
            vcpu_count: Some(plan.run.vcpu_count),
            mem_size_mib: Some(plan.run.mem_size_mib as usize),
            virtual_time: plan
                .run
                .virtual_time
                .as_ref()
                .map(|time| VirtualTimeConfig {
                    tick_ns: time.tick_ns,
                    exits_per_tick: time.exits_per_tick as u64,
                }),
            ..Default::default()
        })
        .map_err(|error| error.to_string())?;
    resources
        .entropy
        .insert(EntropyDeviceConfig {
            rate_limiter: None,
            seed: Some(plan.run.seed),
            script: None,
        })
        .map_err(|error| error.to_string())?;
    resources.serial_out_path = serial_log;
    Ok(resources)
}

fn explorer_config(
    plan: &ExplorePlan,
    serial_log_dir: Option<PathBuf>,
) -> Result<ExplorerConfig, String> {
    let events = plan
        .events_hex
        .iter()
        .map(|event| u8::from_str_radix(event, 16).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ExplorerConfig {
        events,
        branch_event_suffix: plan.branch_event_suffix,
        rendezvous: plan.rendezvous,
        faults: None,
        run_ms: plan.run_ms,
        branches_per_node: plan.branches_per_node as usize,
        max_depth: plan.max_depth,
        max_nodes: plan.max_nodes as usize,
        novelty: match plan.novelty {
            Novelty::Markers => NoveltyStrategy::Markers,
            Novelty::Coverage => NoveltyStrategy::DirtyPages,
        },
        serial_log_dir,
    })
}

fn serial_log_path(directory: &Path, seed: u64) -> PathBuf {
    directory.join(format!("{seed}.log"))
}

fn lock_plan(mut plan: RunPlan, output: &Path) -> Result<RunPlan, String> {
    plan.runtime.firecracker = lock_artifact(output, "firecracker", &plan.runtime.firecracker)?;
    plan.guest.kernel = lock_artifact(output, "kernel", &plan.guest.kernel)?;
    plan.guest.initramfs = lock_artifact(output, "initramfs", &plan.guest.initramfs)?;
    Ok(plan)
}

fn write_plan(output: &Path, plan: &RunPlan) -> Result<(), String> {
    fs::write(
        output.join("explore-plan.json"),
        serde_json::to_vec_pretty(plan).unwrap(),
    )
    .map_err(|error| error.to_string())
}

fn lock_artifact(
    output: &Path,
    name: &str,
    artifact: &ArtifactPlan,
) -> Result<ArtifactPlan, String> {
    let bytes = fs::read(&artifact.path)
        .map_err(|error| format!("cannot read {}: {error}", artifact.path))?;
    if hex(Sha256::digest(&bytes)) != artifact.sha256 {
        return Err(format!("artifact digest changed: {}", artifact.path));
    }
    let target = output.join("artifacts").join(name);
    fs::write(&target, bytes).map_err(|error| error.to_string())?;
    Ok(ArtifactPlan {
        path: target.display().to_string(),
        sha256: artifact.sha256.clone(),
    })
}

fn write_result(
    output: &Path,
    status: &'static str,
    error: Option<String>,
    checks: Vec<CheckResult>,
    nodes: Vec<NodeRecord>,
    minimization: Option<Minimization>,
    replay_verification: Option<ReplayVerification>,
) -> Result<(), String> {
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&ResultRecord {
            format: "theseus-exploration-result-v1",
            status,
            error,
            checks,
            nodes,
            minimization,
            replay_verification,
        })
        .unwrap(),
    )
    .map_err(|error| error.to_string())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(search_index: usize, seed_path: Vec<u64>, markers_hex: &str) -> NodeRecord {
        let seed = *seed_path.last().unwrap();
        NodeRecord {
            search_index,
            id: search_index as NodeId,
            parent: None,
            depth: seed_path.len().saturating_sub(1) as u32,
            seed,
            seed_path,
            entropy_probe_hex: String::new(),
            markers_hex: markers_hex.to_owned(),
            dirty_pages: None,
            serial_log: format!("serial/{seed}.log"),
            serial: Vec::new(),
        }
    }

    #[test]
    fn marker_properties_apply_to_every_captured_timeline() {
        let nodes = vec![node(0, vec![42], "4290ff"), node(1, vec![42, 7], "90ff")];
        let checks = vec![
            CheckPlan {
                name: "completed".to_owned(),
                kind: CheckKind::MarkerSeen,
                value: "ff".to_owned(),
            },
            CheckPlan {
                name: "no error".to_owned(),
                kind: CheckKind::MarkerNotSeen,
                value: "ee".to_owned(),
            },
        ];

        let result = evaluate_checks(&checks, &nodes).unwrap();
        assert!(result.iter().all(|check| check.status == "passed"));
    }

    #[test]
    fn reports_the_seed_path_that_violated_a_marker_property() {
        let nodes = vec![node(0, vec![42], "42ff"), node(1, vec![42, 7], "90")];
        let checks = vec![CheckPlan {
            name: "completed".to_owned(),
            kind: CheckKind::MarkerSeen,
            value: "ff".to_owned(),
        }];

        let result = evaluate_checks(&checks, &nodes).unwrap();
        assert_eq!(result[0].status, "failed");
        assert!(result[0].detail.contains("#1 [42, 7]"));
    }

    #[test]
    fn serial_properties_apply_to_every_captured_timeline() {
        let mut nodes = vec![node(0, vec![42], "ff"), node(1, vec![42, 7], "ff")];
        nodes[0].serial = b"booted\nfinished\n".to_vec();
        nodes[1].serial = b"booted\nfinished\n".to_vec();
        let checks = vec![
            CheckPlan {
                name: "finished".to_owned(),
                kind: CheckKind::SerialContains,
                value: "finished".to_owned(),
            },
            CheckPlan {
                name: "no panic".to_owned(),
                kind: CheckKind::SerialNotContains,
                value: "panic".to_owned(),
            },
        ];

        let result = evaluate_checks(&checks, &nodes).unwrap();
        assert!(result.iter().all(|check| check.status == "passed"));

        nodes[1].serial.clear();
        let result = evaluate_checks(&checks[..1], &nodes).unwrap();
        assert_eq!(result[0].status, "failed");
        assert!(result[0].detail.contains("#1 [42, 7]"));
    }

    #[test]
    fn event_reduction_is_deterministic_and_one_minimal() {
        let minimized = reduce_events(
            ["01", "02", "03", "04"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            |events| {
                events.iter().any(|event| event == "02") && events.iter().any(|event| event == "03")
            },
        );
        assert_eq!(minimized, vec!["02".to_owned(), "03".to_owned()]);
    }

    #[test]
    fn targeted_replay_verifies_every_recorded_fingerprint() {
        let mut recorded = node(0, vec![42, 7], "42ff");
        recorded.entropy_probe_hex = "aa".to_owned();
        recorded.dirty_pages = Some(3);
        recorded.serial = b"ready\n".to_vec();
        let expected = ReplayFingerprint {
            entropy_probe_hex: "aa".to_owned(),
            markers_hex: "42ff".to_owned(),
            dirty_pages: Some(3),
            serial_sha256: Some(hex(Sha256::digest(b"ready\n"))),
        };
        assert_eq!(verify_fingerprint(&expected, &recorded).status, "passed");

        recorded.serial.clear();
        assert_eq!(verify_fingerprint(&expected, &recorded).status, "failed");
        recorded.serial = b"ready\n".to_vec();
        recorded.markers_hex = "ff".to_owned();
        assert_eq!(verify_fingerprint(&expected, &recorded).status, "failed");
    }

    #[test]
    fn whole_tree_replay_verifies_paths_and_fingerprints() {
        let mut root = node(0, vec![42], "42ff");
        root.entropy_probe_hex = "aa".to_owned();
        root.dirty_pages = Some(3);
        let mut child = node(1, vec![42, 7], "90ff");
        child.entropy_probe_hex = "bb".to_owned();
        child.dirty_pages = Some(5);
        let expected = vec![
            ReplayTreeNode {
                seed_path: vec![42],
                fingerprint: ReplayFingerprint {
                    entropy_probe_hex: "aa".to_owned(),
                    markers_hex: "42ff".to_owned(),
                    dirty_pages: Some(3),
                    serial_sha256: None,
                },
            },
            ReplayTreeNode {
                seed_path: vec![42, 7],
                fingerprint: ReplayFingerprint {
                    entropy_probe_hex: "bb".to_owned(),
                    markers_hex: "90ff".to_owned(),
                    dirty_pages: Some(5),
                    serial_sha256: None,
                },
            },
        ];
        assert_eq!(verify_tree(&expected, &[root, child]).status, "passed");

        let short = &expected[..1];
        assert_eq!(
            verify_tree(short, &[node(0, vec![42], "42ff")]).status,
            "failed"
        );
    }
}
