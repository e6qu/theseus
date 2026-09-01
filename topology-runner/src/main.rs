// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Linux+KVM executor for a normalized Theseus Compose topology plan.
//!
//! This stays separate from the portable `theseus` CLI: the CLI plans on
//! macOS, while this binary links Firecracker's Linux/KVM VMM and runs only
//! from a published Linux runtime bundle.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use theseus_engine::simnet::{SharedSimSwitch, SimSwitch};
use vmm::builder::build_microvm_for_boot;
use vmm::devices::virtio::net::{Net, SimNetConfig};
use vmm::rate_limiter::RateLimiter;
use vmm::resources::VmResources;
use vmm::seccomp::get_empty_filters;
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::entropy::EntropyDeviceConfig;
use vmm::vmm_config::instance_info::InstanceInfo;
use vmm::vmm_config::machine_config::{MachineConfigUpdate, VirtualTimeConfig};
use vmm::{EventManager, FcExitCode, Vmm};

const USAGE: &str = "Usage: theseus-topology --plan topology-plan.json --output replay-dir";

#[derive(Debug, Deserialize, Serialize)]
struct TopologyPlan {
    format: String,
    compose: String,
    services: BTreeMap<String, ServicePlan>,
    networks: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ServicePlan {
    manifest: String,
    run: RunPlan,
    networks: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RunPlan {
    format: String,
    manifest: String,
    runtime: RuntimePlan,
    guest: GuestPlan,
    run: RunConfig,
    #[serde(default)]
    network: NetworkConfig,
    #[serde(default)]
    events: Vec<serde_json::Value>,
    #[serde(default)]
    checks: Vec<CheckPlan>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RuntimePlan {
    firecracker: Artifact,
}
#[derive(Debug, Deserialize, Serialize)]
struct GuestPlan {
    kernel: Artifact,
    initramfs: Artifact,
}
#[derive(Debug, Deserialize, Serialize)]
struct Artifact {
    path: String,
    sha256: String,
}
#[derive(Debug, Deserialize, Serialize)]
struct RunConfig {
    seed: u64,
    vcpu_count: u8,
    mem_size_mib: u32,
    timeout_secs: u64,
    virtual_time: Option<VirtualTime>,
}
#[derive(Debug, Deserialize, Serialize)]
struct VirtualTime {
    tick_ns: u64,
    exits_per_tick: u32,
}
#[derive(Debug, Default, Deserialize, Serialize)]
struct NetworkConfig {
    loopback: bool,
    drop_ppm: u32,
    partitioned: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct CheckPlan {
    name: String,
    kind: CheckKind,
    value: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckKind {
    SerialContains,
    SerialNotContains,
    MarkerSeen,
}

#[derive(Debug, Serialize)]
struct CheckResult {
    name: String,
    status: &'static str,
    detail: String,
}

#[derive(Debug, Serialize)]
struct ServiceResult {
    status: &'static str,
    serial_log: String,
    error: Option<String>,
    checks: Vec<CheckResult>,
}

struct ServiceVm {
    vmm: Arc<Mutex<Vmm>>,
    event_manager: EventManager,
}

impl ServiceVm {
    fn pump(&mut self) {
        let _ = self.event_manager.run_with_timeout(0);
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .pump_simulated_network();
    }
    fn exited(&self) -> Option<FcExitCode> {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .shutdown_exit_code()
    }
    fn stop(&self) {
        self.vmm
            .lock()
            .expect("VMM lock poisoned")
            .stop(FcExitCode::Ok);
    }
}

fn main() -> std::process::ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("theseus-topology: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let [flag_plan, plan, flag_output, output] = args.as_slice() else {
        return Err(USAGE.to_owned());
    };
    if flag_plan != "--plan" || flag_output != "--output" {
        return Err(USAGE.to_owned());
    }
    let input = fs::read_to_string(plan).map_err(|error| format!("cannot read {plan}: {error}"))?;
    let topology: TopologyPlan = serde_json::from_str(&input)
        .map_err(|error| format!("cannot parse topology plan: {error}"))?;
    if topology.format != "theseus-compose-plan-v1" || topology.services.is_empty() {
        return Err("unsupported or empty topology plan".to_owned());
    }
    let output = PathBuf::from(output);
    if output.exists() {
        return Err(format!(
            "replay output already exists: {}",
            output.display()
        ));
    }
    fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    fs::write(
        output.join("replay-plan.json"),
        serde_json::to_vec_pretty(&topology).unwrap(),
    )
    .map_err(|error| error.to_string())?;
    execute(topology, &output)
}

fn execute(topology: TopologyPlan, output: &Path) -> Result<(), String> {
    let mut switches: BTreeMap<String, SharedSimSwitch> = topology
        .networks
        .keys()
        .map(|name| (name.clone(), Arc::new(Mutex::new(SimSwitch::new()))))
        .collect();
    let mut services = BTreeMap::new();
    let mut logs = BTreeMap::new();
    for (name, service) in &topology.services {
        if !service.run.events.is_empty() {
            return Err(format!(
                "service {name:?} has serial events; topology serial injection is not available yet"
            ));
        }
        let service_dir = output.join("services").join(name);
        fs::create_dir_all(service_dir.join("artifacts")).map_err(|error| error.to_string())?;
        let kernel = lock_artifact(&service_dir, "kernel", &service.run.guest.kernel)?;
        let initramfs = lock_artifact(&service_dir, "initramfs", &service.run.guest.initramfs)?;
        let _runtime = lock_artifact(
            &service_dir,
            "firecracker",
            &service.run.runtime.firecracker,
        )?;
        let serial = service_dir.join("serial.log");
        let vm = build_service(name, service, &kernel, &initramfs, &serial, &mut switches)?;
        logs.insert(name.clone(), serial);
        services.insert(name.clone(), vm);
    }
    for service in services.values() {
        service
            .vmm
            .lock()
            .unwrap()
            .resume_vm()
            .map_err(|error| error.to_string())?;
    }
    let timeout = topology
        .services
        .values()
        .map(|service| service.run.run.timeout_secs)
        .max()
        .unwrap_or(1);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while Instant::now() < deadline && services.values().any(|service| service.exited().is_none()) {
        for service in services.values_mut() {
            service.pump();
        }
    }
    let mut failed = false;
    for (name, service) in &services {
        let exit = service.exited();
        let (exit_status, error) = match exit {
            Some(FcExitCode::Ok) => ("passed", None),
            Some(code) => ("failed", Some(format!("guest exited with {code:?}"))),
            None => ("failed", Some("guest did not exit before topology timeout".to_owned())),
        };
        let mut checks = evaluate_checks(&topology.services[name].run.checks, &logs[name]);
        checks.insert(0, CheckResult {
            name: "guest_exit".to_owned(),
            status: exit_status,
            detail: error.clone().unwrap_or_else(|| "guest exited with status 0".to_owned()),
        });
        let status = if checks.iter().all(|check| check.status == "passed") { "passed" } else { "failed" };
        if status == "failed" { failed = true; }
        let result = ServiceResult { status, serial_log: logs[name].display().to_string(), error, checks };
        fs::write(
            output.join("services").join(name).join("result.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .map_err(|error| error.to_string())?;
        service.stop();
    }
    if failed {
        Err(format!(
            "one or more services failed; inspect {}/services",
            output.display()
        ))
    } else {
        Ok(())
    }
}

fn evaluate_checks(checks: &[CheckPlan], serial: &Path) -> Vec<CheckResult> {
    let serial = fs::read(serial).unwrap_or_default();
    checks
        .iter()
        .map(|check| {
            let needle = match check.kind {
                CheckKind::MarkerSeen => format!("THES:M:{}", check.value).into_bytes(),
                _ => check.value.as_bytes().to_vec(),
            };
            let contains = serial.windows(needle.len()).any(|window| window == needle);
            let passed = match check.kind {
                CheckKind::SerialContains | CheckKind::MarkerSeen => contains,
                CheckKind::SerialNotContains => !contains,
            };
            CheckResult {
                name: check.name.clone(),
                status: if passed { "passed" } else { "failed" },
                detail: if passed { "serial log satisfied check".to_owned() } else { "serial log did not satisfy check".to_owned() },
            }
        })
        .collect()
}

fn build_service(
    name: &str,
    service: &ServicePlan,
    kernel: &Path,
    initramfs: &Path,
    serial: &Path,
    switches: &mut BTreeMap<String, SharedSimSwitch>,
) -> Result<ServiceVm, String> {
    let mut resources = VmResources::default();
    resources
        .build_boot_source(BootSourceConfig {
            kernel_image_path: kernel.display().to_string(),
            initrd_path: Some(initramfs.display().to_string()),
            boot_args: Some("console=ttyS0 reboot=k panic=-1".to_owned()),
        })
        .map_err(|error| error.to_string())?;
    resources
        .update_machine_config(&MachineConfigUpdate {
            vcpu_count: Some(service.run.run.vcpu_count),
            mem_size_mib: Some(service.run.run.mem_size_mib as usize),
            virtual_time: service
                .run
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
            seed: Some(service.run.run.seed),
            script: None,
        })
        .map_err(|error| error.to_string())?;
    resources.serial_out_path = Some(serial.to_path_buf());
    for network in &service.networks {
        let switch = switches
            .get(network)
            .ok_or_else(|| format!("service {name}: unknown network {network}"))?
            .clone();
        let net = Net::new_with_sim_switch(
            format!("net-{network}"),
            SimNetConfig {
                seed: service.run.run.seed,
                loopback: service.run.network.loopback,
                drop_ppm: service.run.network.drop_ppm,
                partitioned: service.run.network.partitioned,
            },
            switch,
            format!("{network}/{name}"),
            None,
            RateLimiter::default(),
            RateLimiter::default(),
            None,
        )
        .map_err(|error| error.to_string())?;
        resources.net_builder.add_device(Arc::new(Mutex::new(net)));
    }
    let mut event_manager = EventManager::new().map_err(|error| error.to_string())?;
    let vmm = build_microvm_for_boot(
        &InstanceInfo::default(),
        &resources,
        &mut event_manager,
        &get_empty_filters(),
    )
    .map_err(|error| error.to_string())?;
    Ok(ServiceVm { vmm, event_manager })
}

fn lock_artifact(service_dir: &Path, name: &str, artifact: &Artifact) -> Result<PathBuf, String> {
    let source = Path::new(&artifact.path);
    let bytes =
        fs::read(source).map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if digest != artifact.sha256 {
        return Err(format!("artifact digest changed: {}", source.display()));
    }
    let target = service_dir.join("artifacts").join(name);
    fs::copy(source, &target).map_err(|error| error.to_string())?;
    Ok(target)
}
