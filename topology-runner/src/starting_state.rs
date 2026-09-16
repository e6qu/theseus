// Copyright 2026 Adrian Mârza and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Durable topology roots. Boot is inherited, never claimed to be replayed.

use super::*;
use std::io::Write;
use vmm::checkpoint::{artifact, CheckpointArtifact, ExecutionDeviceState};
use vmm::vstate::vcpu::CheckpointExecutionState;

const MAX_CONTEXT: u64 = 128 * 1024 * 1024;
const MAX_MEMORY: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionStart {
    kind: String,
    checkpoint_sha256: String,
    inherited_decisions: u64,
}

pub(super) fn origin(
    topology: &TopologyPlan,
    _root: Option<&CampaignCheckpoint>,
    name: &str,
) -> Option<ExecutionStart> {
    Some(ExecutionStart {
        kind: "topology_checkpoint".to_owned(),
        checkpoint_sha256: topology.starting_checkpoint.as_ref()?.sha256.clone(),
        inherited_decisions: *topology.checkpoint_prefixes.get(name)?,
    })
}

pub(super) fn check_recorded_origin(
    topology: &TopologyPlan,
    root: &CampaignCheckpoint,
    plan: &Path,
) -> Result<(), String> {
    for name in topology.services.keys() {
        let path = plan
            .parent()
            .ok_or("missing replay bundle")?
            .join("services")
            .join(name)
            .join("result.json");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        let recorded: ExecutionStart = serde_json::from_value(value["execution_start"].clone())
            .map_err(|_| format!("recorded service {name:?} is missing its checkpoint origin"))?;
        if Some(recorded) != origin(topology, Some(root), name) {
            return Err(format!("recorded checkpoint origin differs for {name:?}"));
        }
        let trace: Vec<String> = serde_json::from_value(value["machine_execution_trace"].clone())
            .map_err(|_| "missing recorded machine trace".to_owned())?;
        let (machine, ledgers) = CheckpointExecutionState {
            trace,
            pending_interrupts: Vec::new(),
        }
        .restore(usize::from(topology.services[name].run.run.vcpu_count))?;
        if serde_json::to_value(machine.ledger_evidence()).map_err(|error| error.to_string())?
            != value["machine_execution_ledger"]
            || serde_json::to_value(
                ledgers
                    .iter()
                    .map(ExecutionLedger::evidence)
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| error.to_string())?
                != value["execution_ledgers"]
        {
            return Err(format!("recorded execution hashes or tails for {name:?} do not cover the complete retained stream"));
        }
    }
    Ok(())
}

pub(super) fn check_recorded_contract(topology: &TopologyPlan, plan: &Path) -> Result<(), String> {
    for name in topology.services.keys() {
        let path = plan
            .parent()
            .ok_or("missing replay bundle")?
            .join("services")
            .join(name)
            .join("result.json");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        if !value["execution_start"].is_null() && topology.starting_checkpoint.is_none() {
            return Err(
                "checkpoint execution evidence cannot be downgraded to fresh_boot".to_owned(),
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReplayStart {
    #[default]
    FreshBoot,
    ReadyCheckpoint,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    architecture: String,
    configuration_sha256: String,
    context: CheckpointArtifact,
    services: BTreeMap<String, Members>,
    execution_prefixes: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Members {
    vmstate: CheckpointArtifact,
    memory: CheckpointArtifact,
}

#[derive(Deserialize, Serialize)]
struct Context {
    switches: BTreeMap<String, SimSwitchState>,
    services: BTreeMap<String, StoredService>,
    round: u64,
}

#[derive(Deserialize, Serialize)]
struct StoredService {
    networks: BTreeMap<String, SimNetState>,
    serial_contents: Vec<Vec<u8>>,
    serial_pending_bytes: usize,
    program_counters: Vec<u64>,
    next_fault: usize,
    paused_until: Option<u64>,
    faults: Vec<AppliedFault>,
    network_traffic: BTreeMap<String, NetworkTraffic>,
    network_trace: BTreeMap<String, Vec<NetworkFrame>>,
    storage_sha256: BTreeMap<String, String>,
    virtual_time_ns: Option<Vec<u64>>,
    private_dirty_pages: Option<u64>,
    execution_locations: Vec<Vec<u64>>,
    execution: CheckpointExecutionState,
    devices: ExecutionDeviceState,
}

/// Bind boot/device configuration without tying a root to its capture path.
/// Campaign schedules may change; their underlying VM configuration may not.
fn configuration(topology: &TopologyPlan) -> Result<String, String> {
    let mut services = serde_json::Map::new();
    for (name, service) in &topology.services {
        let mut run = serde_json::to_value(&service.run).map_err(|error| error.to_string())?;
        for key in ["manifest", "events", "checks"] {
            run.as_object_mut().unwrap().remove(key);
        }
        fn strip_paths(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.contains_key("sha256") {
                        map.remove("path");
                    }
                    for child in map.values_mut() {
                        strip_paths(child);
                    }
                }
                serde_json::Value::Array(values) => {
                    for child in values {
                        strip_paths(child);
                    }
                }
                _ => {}
            }
        }
        strip_paths(&mut run);
        services.insert(
            name.clone(),
            serde_json::json!({"run": run, "networks": service.networks}),
        );
    }
    let bytes = serde_json::to_vec(&serde_json::json!({
        "services": services, "networks": topology.networks,
        "runtime": topology.topology_runner.as_ref().map(|runner| &runner.sha256),
    }))
    .map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())
}

fn checked(path: &Path, expected: &CheckpointArtifact, maximum: u64) -> Result<(), String> {
    let actual = artifact(path, maximum).map_err(|error| error.to_string())?;
    if actual.bytes != expected.bytes || actual.sha256 != expected.sha256 {
        return Err(format!(
            "starting checkpoint member changed: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(super) fn retain(
    topology: &TopologyPlan,
    root: &CampaignCheckpoint,
    directory: &Path,
) -> Result<Artifact, String> {
    fs::create_dir(directory).map_err(|error| error.to_string())?;
    let mut stored = BTreeMap::new();
    let mut members = BTreeMap::new();
    let mut execution_prefixes = BTreeMap::new();
    for (name, vm) in &root.services {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
        {
            return Err("unsafe checkpoint service name".to_owned());
        }
        let scheduler = &root.scheduler[name];
        let service = directory.join(name);
        fs::create_dir(&service).map_err(|error| error.to_string())?;
        vm.branch
            .export_snapshot(&service.join("vmstate"), &service.join("memory"))
            .map_err(|error| error.to_string())?;
        for member in ["vmstate", "memory"] {
            fs::File::open(service.join(member))
                .and_then(|file| file.sync_all())
                .map_err(|error| error.to_string())?;
        }
        let execution = scheduler
            .machine_execution_state
            .as_ref()
            .ok_or("checkpoint is missing machine execution state")?
            .checkpoint_state();
        execution_prefixes.insert(name.clone(), execution.trace.clone());
        if execution
            .trace
            .last()
            .is_some_and(|record| record.ends_with(":pio_write:0x64:1:fe"))
        {
            return Err(format!("service {name:?} exited before capture; use a guest that waits for input after readiness"));
        }
        members.insert(
            name.clone(),
            Members {
                vmstate: artifact(&service.join("vmstate"), MAX_CONTEXT)
                    .map_err(|error| error.to_string())?,
                memory: artifact(&service.join("memory"), MAX_MEMORY)
                    .map_err(|error| error.to_string())?,
            },
        );
        stored.insert(
            name.clone(),
            StoredService {
                networks: vm.networks.clone(),
                serial_contents: scheduler.serial_contents.clone(),
                serial_pending_bytes: scheduler.serial_pending_bytes,
                program_counters: scheduler.program_counters.clone(),
                next_fault: scheduler.next_fault,
                paused_until: scheduler.paused_until,
                faults: scheduler.faults.clone(),
                network_traffic: scheduler.network_traffic.clone(),
                network_trace: scheduler.network_trace.clone(),
                storage_sha256: scheduler.storage_sha256.clone(),
                virtual_time_ns: scheduler.virtual_time_ns.clone(),
                private_dirty_pages: scheduler.private_dirty_pages,
                execution_locations: scheduler
                    .execution_locations
                    .clone()
                    .ok_or("checkpoint is missing execution locations")?,
                execution,
                devices: scheduler
                    .devices
                    .clone()
                    .ok_or("checkpoint is missing transient devices")?,
            },
        );
    }
    let context = bitcode::serialize(&Context {
        switches: root.switches.clone(),
        services: stored,
        round: root.round,
    })
    .map_err(|error| error.to_string())?;
    if context.len() as u64 > MAX_CONTEXT {
        return Err("starting checkpoint context exceeds 128 MiB".to_owned());
    }
    write_new(&directory.join("context.bin"), &context)?;
    let metadata = Manifest {
        format: "theseus-topology-checkpoint-v1".to_owned(),
        architecture: runtime_architecture()?.to_owned(),
        configuration_sha256: configuration(topology)?,
        services: members,
        execution_prefixes,
        context: artifact(&directory.join("context.bin"), MAX_CONTEXT)
            .map_err(|error| error.to_string())?,
    };
    let manifest = directory.join("metadata.json");
    write_new(
        &manifest,
        &serde_json::to_vec_pretty(&metadata).map_err(|error| error.to_string())?,
    )?;
    Ok(Artifact {
        path: fs::canonicalize(&manifest)
            .map_err(|error| error.to_string())?
            .display()
            .to_string(),
        sha256: artifact(&manifest, MAX_CONTEXT)
            .map_err(|error| error.to_string())?
            .sha256,
    })
}

/// Verify every retained member and restore rolling hashes BEFORE creating VMs.
pub(super) fn load(
    topology: &TopologyPlan,
    locked: &Artifact,
) -> Result<CampaignCheckpoint, String> {
    let path = Path::new(&locked.path);
    let directory = path.parent().ok_or("starting checkpoint has no parent")?;
    if path.file_name().and_then(|name| name.to_str()) != Some("metadata.json")
        || fs::symlink_metadata(directory)
            .map_err(|error| error.to_string())?
            .file_type()
            .is_symlink()
    {
        return Err("unsafe starting checkpoint directory".to_owned());
    }
    let actual = artifact(path, MAX_CONTEXT).map_err(|error| error.to_string())?;
    if actual.sha256 != locked.sha256 {
        return Err("starting checkpoint identity changed".to_owned());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    if format!("{:x}", Sha256::digest(&bytes)) != locked.sha256 {
        return Err("starting checkpoint changed during read".to_owned());
    }
    let metadata: Manifest = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if metadata.format != "theseus-topology-checkpoint-v1"
        || metadata.architecture != runtime_architecture()?
        || metadata.configuration_sha256 != configuration(topology)?
        || metadata.services.keys().ne(topology.services.keys())
        || metadata
            .execution_prefixes
            .keys()
            .ne(topology.services.keys())
    {
        return Err(
            "starting checkpoint format, architecture, service set, or VM configuration differs"
                .to_owned(),
        );
    }
    let context_path = directory.join("context.bin");
    checked(&context_path, &metadata.context, MAX_CONTEXT)?;
    let bytes = fs::read(&context_path).map_err(|error| error.to_string())?;
    if format!("{:x}", Sha256::digest(&bytes)) != metadata.context.sha256 {
        return Err("checkpoint context changed during read".to_owned());
    }
    let context: Context = bitcode::deserialize(&bytes)
        .map_err(|error| format!("invalid checkpoint context: {error}"))?;
    if context.services.keys().ne(topology.services.keys())
        || context.switches.keys().ne(topology.networks.keys())
    {
        return Err("checkpoint context service or switch set differs".to_owned());
    }
    let mut services = BTreeMap::new();
    let mut scheduler = BTreeMap::new();
    for (name, state) in context.services {
        if metadata.execution_prefixes.get(&name) != Some(&state.execution.trace) {
            return Err(
                "checkpoint context differs from its inspectable inherited prefix".to_owned(),
            );
        }
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
        {
            return Err("unsafe checkpoint service name".to_owned());
        }
        let profile = &topology.services[&name];
        let members = &metadata.services[&name];
        let service = directory.join(&name);
        if fs::symlink_metadata(&service)
            .map_err(|error| error.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("symlink checkpoint service directory".to_owned());
        }
        checked(&service.join("vmstate"), &members.vmstate, MAX_CONTEXT)?;
        checked(&service.join("memory"), &members.memory, MAX_MEMORY)?;
        if members.memory.bytes != u64::from(profile.run.run.mem_size_mib) * 1024 * 1024
            || state.networks.keys().collect::<BTreeSet<_>>()
                != profile.networks.iter().collect::<BTreeSet<_>>()
            || state.program_counters.len() != usize::from(profile.run.run.vcpu_count)
            || state.serial_contents.is_empty()
            || state.serial_pending_bytes > 64
            || state.devices.keyboard.is_some() != cfg!(target_arch = "x86_64")
            || state
                .devices
                .keyboard
                .as_ref()
                .is_some_and(|keyboard| keyboard.buffer.len() > 16)
        {
            return Err(format!(
                "invalid checkpoint devices, memory, or scheduler state for {name}"
            ));
        }
        let (machine, ledgers) = state
            .execution
            .restore(usize::from(profile.run.run.vcpu_count))?;
        state
            .devices
            .validate()
            .map_err(|error| error.to_string())?;
        let branch = BranchPoint::import_snapshot(
            &service.join("vmstate"),
            &service.join("memory"),
            profile.run.run.seed,
        )
        .map_err(|error| error.to_string())?;
        let microvm = branch.microvm_state().map_err(|error| error.to_string())?;
        if microvm.vm_info.mem_size_mib != u64::from(profile.run.run.mem_size_mib)
            || microvm.vcpu_states.len() != usize::from(profile.run.run.vcpu_count)
        {
            return Err(format!(
                "retained VM state for {name:?} differs from its locked configuration"
            ));
        }
        services.insert(
            name.clone(),
            ServiceVmCheckpoint {
                branch: Arc::new(branch),
                memory_bytes: members.memory.bytes,
                networks: state.networks,
            },
        );
        scheduler.insert(
            name,
            ServiceSchedulerCheckpoint {
                serial_contents: state.serial_contents,
                serial_pending_bytes: state.serial_pending_bytes,
                program_counters: state.program_counters,
                next_fault: state.next_fault,
                paused_until: state.paused_until,
                faults: state.faults,
                network_traffic: state.network_traffic,
                network_trace: state.network_trace,
                storage_sha256: state.storage_sha256,
                virtual_time_ns: state.virtual_time_ns,
                private_dirty_pages: state.private_dirty_pages,
                execution_locations: Some(state.execution_locations),
                execution_ledgers: Some(ledgers),
                machine_execution_state: Some(machine),
                devices: Some(state.devices),
            },
        );
    }
    let prefix_counts = scheduler
        .iter()
        .map(|(name, state)| {
            (
                name.clone(),
                state
                    .machine_execution_state
                    .as_ref()
                    .unwrap()
                    .trace()
                    .len() as u64,
            )
        })
        .collect::<BTreeMap<_, _>>();
    if prefix_counts != topology.checkpoint_prefixes {
        return Err("locked checkpoint ancestry counts differ from retained context".to_owned());
    }
    Ok(CampaignCheckpoint {
        services,
        scheduler,
        switches: context.switches,
        round: context.round,
    })
}

pub(super) fn boot_or_load(
    topology: &mut TopologyPlan,
    directory: &Path,
    driver: &str,
) -> Result<CampaignCheckpoint, String> {
    if let Some(locked) = &topology.starting_checkpoint {
        if topology.replay_start != ReplayStart::ReadyCheckpoint {
            return Err("checkpoint cannot be downgraded to fresh_boot".to_owned());
        }
        return load(topology, locked);
    }
    let root = boot_campaign_checkpoint(topology, directory, driver)?;
    if topology.replay_start == ReplayStart::ReadyCheckpoint {
        topology.checkpoint_prefixes = root
            .scheduler
            .iter()
            .map(|(name, state)| {
                (
                    name.clone(),
                    state
                        .machine_execution_state
                        .as_ref()
                        .unwrap()
                        .trace()
                        .len() as u64,
                )
            })
            .collect();
        topology.starting_checkpoint =
            Some(retain(topology, &root, &directory.join("starting-state"))?);
    }
    Ok(root)
}

pub(super) fn check_prefix(
    root: &CampaignCheckpoint,
    expected: &BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    for (name, scheduler) in &root.scheduler {
        let prefix = scheduler
            .machine_execution_state
            .as_ref()
            .ok_or("missing inherited execution prefix")?
            .trace();
        let trace = expected
            .get(name)
            .ok_or("missing expected machine stream")?;
        if !trace.starts_with(prefix) || trace.len() <= prefix.len() {
            return Err(format!("recorded stream for {name:?} lacks the exact inherited prefix and a resumed suffix"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topology() -> TopologyPlan {
        serde_json::from_value(serde_json::json!({
            "format": "theseus-compose-plan-v1", "compose": "compose.yaml", "networks": {},
            "services": {"api": {"manifest": "theseus.toml", "networks": [], "run": {
                "format": "theseus-run-plan-v1", "manifest": "theseus.toml",
                "runtime": {"firecracker": {"path": "/runtime", "sha256": "a".repeat(64)}},
                "guest": {"kernel": {"path": "/kernel", "sha256": "b".repeat(64)},
                    "initramfs": {"path": "/initrd", "sha256": "c".repeat(64)}},
                "run": {"seed": 42, "vcpu_count": 1, "mem_size_mib": 1, "timeout_secs": 1,
                    "virtual_time": {"tick_ns": 1000, "exits_per_tick": 10}}
            }}}
        }))
        .unwrap()
    }

    #[test]
    fn root_identity_binds_configuration_not_capture_path_or_schedule() {
        let first = topology();
        let digest = configuration(&first).unwrap();
        let mut moved = topology();
        moved.services.get_mut("api").unwrap().run.guest.kernel.path =
            "elsewhere/kernel".to_owned();
        moved.services.get_mut("api").unwrap().run.manifest = "elsewhere/theseus.toml".to_owned();
        assert_eq!(digest, configuration(&moved).unwrap());
        moved.services.get_mut("api").unwrap().run.run.seed += 1;
        assert_ne!(digest, configuration(&moved).unwrap());
        let mut changed = topology();
        changed
            .services
            .get_mut("api")
            .unwrap()
            .run
            .guest
            .kernel
            .sha256 = "d".repeat(64);
        assert_ne!(digest, configuration(&changed).unwrap());
    }

    #[test]
    fn missing_checkpoint_identity_and_corrupt_context_fail_before_restore() {
        let directory = std::env::temp_dir().join(format!(
            "theseus-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let topology = topology();
        let path = directory.join("metadata.json");
        write_new(&directory.join("context.bin"), b"invalid context").unwrap();
        let manifest = Manifest {
            format: "theseus-topology-checkpoint-v1".to_owned(),
            execution_prefixes: BTreeMap::new(),
            architecture: runtime_architecture().unwrap().to_owned(),
            configuration_sha256: configuration(&topology).unwrap(),
            context: artifact(&directory.join("context.bin"), MAX_CONTEXT).unwrap(),
            services: [(
                "api".to_owned(),
                Members {
                    vmstate: CheckpointArtifact {
                        bytes: 1,
                        sha256: "a".repeat(64),
                    },
                    memory: CheckpointArtifact {
                        bytes: 1024 * 1024,
                        sha256: "b".repeat(64),
                    },
                },
            )]
            .into(),
        };
        write_new(&path, &serde_json::to_vec(&manifest).unwrap()).unwrap();
        let locked = Artifact {
            path: path.display().to_string(),
            sha256: artifact(&path, MAX_CONTEXT).unwrap().sha256,
        };
        let wrong = Artifact {
            path: locked.path.clone(),
            sha256: "c".repeat(64),
        };
        assert!(load(&topology, &wrong)
            .err()
            .unwrap()
            .contains("identity changed"));
        assert!(load(&topology, &locked)
            .err()
            .unwrap()
            .contains("invalid checkpoint context"));
        fs::write(directory.join("context.bin"), b"changed").unwrap();
        assert!(load(&topology, &locked)
            .err()
            .unwrap()
            .contains("member changed"));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn binary_context_retains_queues_commands_interrupts_and_execution() {
        let execution = CheckpointExecutionState {
            trace: vec!["host:serial_input:1:2a".to_owned()],
            pending_interrupts: vec![vmm::vstate::vcpu::CheckpointInterrupt {
                source: "serial".to_owned(),
                gsi: 4,
                coalesce: true,
            }],
        };
        let devices = ExecutionDeviceState {
            control: theseus_engine::door::ControlState {
                host_events: [42].into(),
                event_log: vec![
                    theseus_engine::door::ControlEvent::SetupComplete,
                    theseus_engine::door::ControlEvent::GuestLog(17),
                ],
            },
            keyboard: None,
        };
        let mut nic = theseus_engine::simnet::SimNet::new(theseus_engine::simnet::SimNetConfig {
            loopback: true,
            latency_rounds: 2,
            ..Default::default()
        });
        nic.write_frame(b"queued");
        let state = StoredService {
            networks: [("backplane".to_owned(), nic.save_state())].into(),
            serial_contents: vec![b"THES:M:42\n".to_vec()],
            serial_pending_bytes: 1,
            program_counters: vec![0x1000],
            next_fault: 2,
            paused_until: Some(12),
            faults: Vec::new(),
            network_traffic: BTreeMap::new(),
            network_trace: BTreeMap::new(),
            storage_sha256: BTreeMap::new(),
            virtual_time_ns: Some(vec![1000]),
            private_dirty_pages: Some(1),
            execution_locations: vec![vec![0x1000]],
            execution,
            devices,
        };
        let context = Context {
            switches: [("backplane".to_owned(), SimSwitch::new().save_state())].into(),
            services: [("api".to_owned(), state)].into(),
            round: 11,
        };
        let bytes = bitcode::serialize(&context).unwrap();
        let restored: Context = bitcode::deserialize(&bytes).unwrap();
        assert_eq!(restored.round, 11);
        let state = &restored.services["api"];
        assert_eq!(state.next_fault, 2);
        assert_eq!(state.paused_until, Some(12));
        assert_eq!(state.devices.control.host_events, [42]);
        assert_eq!(state.devices.control.event_log.len(), 2);
        assert_eq!(state.execution.pending_interrupts[0].gsi, 4);
        let (machine, _) = state.execution.restore(1).unwrap();
        assert_eq!(machine.trace(), ["host:serial_input:1:2a"]);
        nic.restore_state(state.networks["backplane"].clone());
        nic.advance_round();
        nic.advance_round();
        let mut bytes = [0; 16];
        assert_eq!(nic.read_frame(&mut bytes), Some(6));
        assert_eq!(&bytes[..6], b"queued");
        assert_eq!(nic.stats().tx_sha256, nic.stats().rx_sha256);
    }
}
