// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Linux+KVM executor for a normalized Theseus exploration plan.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use event_manager::EventManager;
use serde::Serialize;
use sha2::{Digest, Sha256};
use theseus_cli::{ArtifactPlan, CheckKind, CheckPlan, ExplorePlan, Novelty, RunPlan};
use theseus_orchestrator::orchestrator::explorer::{
    Explorer, ExplorerConfig, NoveltyStrategy,
};
use theseus_orchestrator::orchestrator::tree::NodeId;
use vmm::builder::build_microvm_for_boot;
use vmm::resources::VmResources;
use vmm::seccomp::get_empty_filters;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::entropy::EntropyDeviceConfig;
use vmm::vmm_config::instance_info::InstanceInfo;
use vmm::vmm_config::machine_config::{MachineConfigUpdate, VirtualTimeConfig};

const USAGE: &str = "Usage: theseus-explorer --plan explore-plan.json --output exploration-dir";

#[derive(Serialize)]
struct ResultRecord {
    format: &'static str,
    status: &'static str,
    error: Option<String>,
    checks: Vec<CheckResult>,
    nodes: Vec<NodeRecord>,
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
}

struct Execution {
    nodes: Vec<NodeRecord>,
    checks: Vec<CheckResult>,
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
    let [flag_plan, plan_path, flag_output, output] = args.as_slice() else {
        return Err(USAGE.to_owned());
    };
    if flag_plan != "--plan" || flag_output != "--output" {
        return Err(USAGE.to_owned());
    }
    let plan: RunPlan = serde_json::from_slice(
        &fs::read(plan_path).map_err(|error| format!("cannot read {plan_path}: {error}"))?,
    )
    .map_err(|error| format!("cannot parse exploration plan: {error}"))?;
    let output = PathBuf::from(output);
    if output.exists() {
        return Err(format!("exploration output already exists: {}", output.display()));
    }
    fs::create_dir_all(output.join("artifacts")).map_err(|error| error.to_string())?;
    let plan = lock_plan(plan, &output)?;
    fs::write(
        output.join("explore-plan.json"),
        serde_json::to_vec_pretty(&plan).unwrap(),
    )
    .map_err(|error| error.to_string())?;

    let result = execute(&plan);
    match result {
        Ok(execution) => {
            let failed = execution
                .checks
                .iter()
                .filter(|check| check.status == "failed")
                .map(|check| check.name.clone())
                .collect::<Vec<_>>();
            write_result(
                &output,
                if failed.is_empty() {
                    "passed"
                } else {
                    "failed"
                },
                None,
                execution.checks,
                execution.nodes,
            )?;
            if failed.is_empty() {
                Ok(())
            } else {
                Err(format!("checks failed: {}", failed.join(", ")))
            }
        }
        Err(error) => {
            write_result(
                &output,
                "failed",
                Some(error.clone()),
                Vec::new(),
                Vec::new(),
            )?;
            Err(error)
        }
    }
}

fn execute(plan: &RunPlan) -> Result<Execution, String> {
    let explore = plan
        .explore
        .as_ref()
        .ok_or_else(|| "exploration plan has no [explore] contract".to_owned())?;
    if !explore.rendezvous {
        return Err("exploration requires explore.rendezvous = true; host-time runs are not replayable".to_owned());
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
    let resources = resources_from_plan(plan)?;
    let config = explorer_config(explore)?;
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
            }
        })
        .collect::<Vec<_>>();
    let checks = evaluate_checks(&plan.checks, &nodes)?;
    Ok(Execution { nodes, checks })
}

/// Exploration has no serial-log transport. A marker property therefore
/// applies to every captured timeline's raw control-channel marker stream.
fn evaluate_checks(checks: &[CheckPlan], nodes: &[NodeRecord]) -> Result<Vec<CheckResult>, String> {
    checks
        .iter()
        .map(|check| {
            let (kind, must_be_present) = match check.kind {
                CheckKind::MarkerSeen => ("marker_seen", true),
                CheckKind::MarkerNotSeen => ("marker_not_seen", false),
                CheckKind::SerialContains | CheckKind::SerialNotContains => {
                    return Err(format!(
                        "check {:?} uses a serial-log kind unavailable during exploration; use marker_seen or marker_not_seen",
                        check.name
                    ));
                }
            };
            let marker = marker_byte(&check.value).map_err(|reason| {
                format!("check {:?} has invalid marker value: {reason}", check.name)
            })?;
            let violating = nodes
                .iter()
                .filter(|node| node.markers_hex.as_bytes().chunks_exact(2).any(|hex| {
                    u8::from_str_radix(std::str::from_utf8(hex).expect("hex is ASCII"), 16)
                        .is_ok_and(|value| value == marker)
                }) != must_be_present)
                .collect::<Vec<_>>();
            if violating.is_empty() {
                Ok(CheckResult {
                    name: check.name.clone(),
                    kind,
                    status: "passed",
                    detail: format!(
                        "all {} captured timelines {} marker {marker:02x}",
                        nodes.len(),
                        if must_be_present { "emitted" } else { "avoided" }
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
                        "{} marker {marker:02x}: {timelines}{}",
                        if must_be_present { "missing" } else { "emitted by" },
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
            CheckKind::SerialContains | CheckKind::SerialNotContains => {
                return Err(format!(
                    "check {:?} uses a serial-log kind unavailable during exploration; use marker_seen or marker_not_seen",
                    check.name
                ));
            }
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

fn resources_from_plan(plan: &RunPlan) -> Result<VmResources, String> {
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
            virtual_time: plan.run.virtual_time.as_ref().map(|time| VirtualTimeConfig {
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
    Ok(resources)
}

fn explorer_config(plan: &ExplorePlan) -> Result<ExplorerConfig, String> {
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
    })
}

fn lock_plan(mut plan: RunPlan, output: &Path) -> Result<RunPlan, String> {
    plan.runtime.firecracker = lock_artifact(output, "firecracker", &plan.runtime.firecracker)?;
    plan.guest.kernel = lock_artifact(output, "kernel", &plan.guest.kernel)?;
    plan.guest.initramfs = lock_artifact(output, "initramfs", &plan.guest.initramfs)?;
    Ok(plan)
}

fn lock_artifact(output: &Path, name: &str, artifact: &ArtifactPlan) -> Result<ArtifactPlan, String> {
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
) -> Result<(), String> {
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&ResultRecord {
            format: "theseus-exploration-result-v1",
            status,
            error,
            checks,
            nodes,
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
        NodeRecord {
            search_index,
            id: search_index as NodeId,
            parent: None,
            depth: seed_path.len().saturating_sub(1) as u32,
            seed: *seed_path.last().unwrap(),
            seed_path,
            entropy_probe_hex: String::new(),
            markers_hex: markers_hex.to_owned(),
            dirty_pages: None,
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
    fn rejects_serial_properties() {
        let checks = vec![CheckPlan {
            name: "serial".to_owned(),
            kind: CheckKind::SerialContains,
            value: "done".to_owned(),
        }];
        assert!(validate_checks(&checks)
            .unwrap_err()
            .contains("serial-log kind unavailable"));
    }
}
