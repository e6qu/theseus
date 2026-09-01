// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Linux+KVM executor for a normalized Theseus exploration plan.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use event_manager::EventManager;
use serde::Serialize;
use sha2::{Digest, Sha256};
use theseus_cli::{ArtifactPlan, ExplorePlan, Novelty, RunPlan};
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
    nodes: Vec<NodeRecord>,
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
        Ok(nodes) => write_result(&output, "passed", None, nodes),
        Err(error) => {
            write_result(&output, "failed", Some(error.clone()), Vec::new())?;
            Err(error)
        }
    }
}

fn execute(plan: &RunPlan) -> Result<Vec<NodeRecord>, String> {
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
    let resources = resources_from_plan(plan)?;
    let config = explorer_config(explore)?;
    let mut event_manager = EventManager::new().map_err(|error| error.to_string())?;
    let filters = get_empty_filters();
    let explorer = Explorer::explore(
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
    .map_err(|error| error.to_string())?;

    Ok(explorer
        .tree
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
        .collect())
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
    nodes: Vec<NodeRecord>,
) -> Result<(), String> {
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&ResultRecord {
            format: "theseus-exploration-result-v1",
            status,
            error,
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
