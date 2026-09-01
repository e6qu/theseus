// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Strict Docker Compose-shaped topology input for Theseus guests.
//!
//! Compose is used here only as a familiar topology notation. Theseus does
//! not accept Docker images, host ports, volumes, or host networks: each
//! service points at its own locked Theseus manifest instead.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::{LoadError, RunPlan, load_plan};

#[derive(Debug)]
pub enum ComposeError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    Invalid(String),
    Manifest {
        service: String,
        source: Box<LoadError>,
    },
}

impl fmt::Display for ComposeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "cannot parse {}: {source}", path.display())
            }
            Self::Invalid(reason) => write!(formatter, "invalid Compose topology: {reason}"),
            Self::Manifest { service, source } => {
                write!(formatter, "service {service:?}: {source}")
            }
        }
    }
}

impl std::error::Error for ComposeError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeFile {
    #[serde(default)]
    name: Option<String>,
    services: BTreeMap<String, ComposeService>,
    #[serde(default)]
    networks: BTreeMap<String, ComposeNetwork>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeNetwork {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeService {
    #[serde(rename = "x-theseus")]
    theseus: ServiceTheseus,
    #[serde(default)]
    networks: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceTheseus {
    manifest: PathBuf,
    #[serde(default)]
    faults: Vec<ComposeFault>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeFault {
    at_round: u64,
    kind: FaultKind,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    Pause,
    Restart,
    ClockJump,
}

#[derive(Debug, Clone, Serialize)]
pub struct FaultPlan {
    pub at_round: u64,
    pub kind: FaultKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_rounds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nanoseconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComposePlan {
    pub format: String,
    pub compose: String,
    pub name: Option<String>,
    pub services: BTreeMap<String, ComposeServicePlan>,
    /// Network name to sorted service names. Every member can exchange frames
    /// once the deterministic multi-guest switch is available.
    pub networks: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComposeServicePlan {
    pub manifest: String,
    pub run: RunPlan,
    pub networks: Vec<String>,
    pub faults: Vec<FaultPlan>,
}

/// Load a Compose topology and lock every referenced service artifact into a
/// normalized plan. Relative paths are rooted at the Compose file and may not
/// escape its directory.
pub fn load_compose_plan(path: impl AsRef<Path>) -> Result<ComposePlan, ComposeError> {
    let path = path.as_ref();
    let compose_path = fs::canonicalize(path).map_err(|source| ComposeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let compose_dir = compose_path.parent().expect("canonical path has a parent");
    let input = fs::read_to_string(&compose_path).map_err(|source| ComposeError::Read {
        path: compose_path.clone(),
        source,
    })?;
    let compose: ComposeFile =
        serde_yaml::from_str(&input).map_err(|source| ComposeError::Parse {
            path: compose_path.clone(),
            source,
        })?;

    if compose.services.is_empty() {
        return Err(ComposeError::Invalid(
            "services must not be empty".to_owned(),
        ));
    }
    if compose.networks.is_empty() {
        return Err(ComposeError::Invalid(
            "declare at least one named network; Theseus never uses a host default network"
                .to_owned(),
        ));
    }

    let mut memberships: BTreeMap<String, BTreeSet<String>> = compose
        .networks
        .keys()
        .map(|name| (name.clone(), BTreeSet::new()))
        .collect();
    let mut services = BTreeMap::new();
    for (name, service) in compose.services {
        validate_name("service", &name)?;
        if service.networks.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} must join at least one named network"
            )));
        }
        let mut networks = BTreeSet::new();
        for network in service.networks {
            validate_name("network", &network)?;
            let Some(members) = memberships.get_mut(&network) else {
                return Err(ComposeError::Invalid(format!(
                    "service {name:?} references undeclared network {network:?}"
                )));
            };
            members.insert(name.clone());
            networks.insert(network);
        }
        if service.theseus.manifest.is_absolute() {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} x-theseus.manifest must be relative to the Compose file"
            )));
        }
        let manifest =
            fs::canonicalize(compose_dir.join(&service.theseus.manifest)).map_err(|source| {
                ComposeError::Read {
                    path: compose_dir.join(&service.theseus.manifest),
                    source,
                }
            })?;
        if !manifest.starts_with(compose_dir) {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} x-theseus.manifest must not escape the Compose directory"
            )));
        }
        let run = load_plan(&manifest).map_err(|source| ComposeError::Manifest {
            service: name.clone(),
            source: Box::new(source),
        })?;
        let faults = validate_faults(
            &name,
            service.theseus.faults,
            run.run.virtual_time.is_some(),
        )?;
        services.insert(
            name,
            ComposeServicePlan {
                manifest: manifest.display().to_string(),
                run,
                networks: networks.into_iter().collect(),
                faults,
            },
        );
    }

    let networks = memberships
        .into_iter()
        .map(|(name, services)| (name, services.into_iter().collect()))
        .collect();
    Ok(ComposePlan {
        format: "theseus-compose-plan-v1".to_owned(),
        compose: compose_path.display().to_string(),
        name: compose.name,
        services,
        networks,
    })
}

fn validate_faults(
    service: &str,
    faults: Vec<ComposeFault>,
    has_virtual_time: bool,
) -> Result<Vec<FaultPlan>, ComposeError> {
    let mut previous_round = 0;
    let mut paused_until = 0;
    let mut plans = Vec::with_capacity(faults.len());
    for fault in faults {
        if fault.at_round == 0 {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} fault at_round must be greater than zero"
            )));
        }
        if fault.at_round <= previous_round {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} faults must use strictly increasing at_round values"
            )));
        }
        if fault.at_round < paused_until {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} fault at round {} falls within a pause ending at round {paused_until}",
                fault.at_round
            )));
        }
        let plan = match fault.kind {
            FaultKind::Pause => {
                let duration = fault.duration_rounds.ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "service {service:?} pause fault requires duration_rounds"
                    ))
                })?;
                if duration == 0 || fault.nanoseconds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} pause fault requires a positive duration_rounds and no nanoseconds"
                    )));
                }
                paused_until = fault.at_round.checked_add(duration).ok_or_else(|| {
                    ComposeError::Invalid(format!("service {service:?} pause duration overflows"))
                })?;
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::Pause,
                    duration_rounds: Some(duration),
                    nanoseconds: None,
                }
            }
            FaultKind::Restart => {
                if fault.duration_rounds.is_some() || fault.nanoseconds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} restart fault takes no duration_rounds or nanoseconds"
                    )));
                }
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::Restart,
                    duration_rounds: None,
                    nanoseconds: None,
                }
            }
            FaultKind::ClockJump => {
                let nanoseconds = fault.nanoseconds.ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "service {service:?} clock_jump fault requires nanoseconds"
                    ))
                })?;
                if !has_virtual_time || nanoseconds == 0 || fault.duration_rounds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} clock_jump requires virtual_time, positive nanoseconds, and no duration_rounds"
                    )));
                }
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::ClockJump,
                    duration_rounds: None,
                    nanoseconds: Some(nanoseconds),
                }
            }
        };
        previous_round = plan.at_round;
        plans.push(plan);
    }
    Ok(plans)
}

fn validate_name(kind: &str, value: &str) -> Result<(), ComposeError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ComposeError::Invalid(format!(
            "{kind} name {value:?} must use only letters, digits, '-' and '_'"
        )));
    }
    Ok(())
}

/// Execute a locked plan with the Linux-only runner shipped beside `theseus`
/// in a published runtime bundle. Planning remains portable because the CLI
/// itself never links the Linux/KVM VMM.
pub fn test_compose(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let plan = load_compose_plan(path)?;
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "replay output already exists: {}",
            output.display()
        )));
    }
    let plan_file = output.with_extension("topology-plan.json");
    if plan_file.exists() {
        return Err(ComposeError::Invalid(format!(
            "temporary topology plan already exists: {}",
            plan_file.display()
        )));
    }
    fs::write(
        &plan_file,
        serde_json::to_vec_pretty(&plan).map_err(|error| {
            ComposeError::Invalid(format!("cannot encode topology plan: {error}"))
        })?,
    )
    .map_err(|source| ComposeError::Read {
        path: plan_file.clone(),
        source,
    })?;
    execute_topology(&plan_file, &output)?;
    let _ = fs::remove_file(&plan_file);
    Ok(output)
}

/// Re-run a recorded topology using its locked service artifacts.
pub fn replay_compose(
    bundle: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| ComposeError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    let plan = bundle.join("replay-plan.json");
    if !plan.is_file() {
        return Err(ComposeError::Invalid(format!(
            "topology replay has no replay-plan.json: {}",
            bundle.display()
        )));
    }
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "replay output already exists: {}",
            output.display()
        )));
    }
    execute_topology(&plan, &output)?;
    Ok(output)
}

fn execute_topology(plan: &Path, output: &Path) -> Result<(), ComposeError> {
    let runner = std::env::current_exe()
        .map_err(|error| ComposeError::Invalid(format!("cannot locate theseus binary: {error}")))?
        .parent()
        .map(|directory| directory.join("theseus-topology"))
        .ok_or_else(|| {
            ComposeError::Invalid("theseus binary has no parent directory".to_owned())
        })?;
    if !runner.is_file() {
        return Err(ComposeError::Invalid(format!(
            "missing Linux topology runner beside theseus: {}; use a published Linux runtime bundle",
            runner.display()
        )));
    }
    let status = Command::new(&runner)
        .arg("--plan")
        .arg(plan)
        .arg("--output")
        .arg(output)
        .status()
        .map_err(|error| {
            ComposeError::Invalid(format!("cannot start {}: {error}", runner.display()))
        })?;
    if !status.success() {
        return Err(ComposeError::Invalid(format!(
            "topology runner failed; inspect {}",
            output.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(compose: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for service in ["api", "worker"] {
            let root = directory.path().join(service);
            fs::create_dir_all(root.join("runtime")).unwrap();
            fs::create_dir_all(root.join("guest")).unwrap();
            fs::write(root.join("runtime/firecracker"), b"firecracker").unwrap();
            #[cfg(unix)]
            fs::set_permissions(
                root.join("runtime/firecracker"),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            fs::write(root.join("guest/vmlinux"), b"kernel").unwrap();
            fs::write(root.join("guest/initramfs.cpio"), b"initramfs").unwrap();
            fs::write(
                root.join("theseus.toml"),
                "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\n[guest]\nkernel = 'guest/vmlinux'\ninitramfs = 'guest/initramfs.cpio'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
            )
            .unwrap();
        }
        fs::write(directory.path().join("compose.yaml"), compose).unwrap();
        directory
    }

    #[test]
    fn locks_service_artifacts_and_links() {
        let directory = fixture(
            "name: example\nservices:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.format, "theseus-compose-plan-v1");
        assert_eq!(plan.networks["backplane"], ["api", "worker"]);
        assert_eq!(plan.services["api"].run.guest.kernel.sha256.len(), 64);
    }

    #[test]
    fn rejects_host_compose_features() {
        let directory = fixture(
            "services:\n  api:\n    image: nginx\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("unknown field `image`"));
    }

    #[test]
    fn rejects_undeclared_network() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [missing]\nnetworks:\n  backplane: {}\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("undeclared network"));
    }

    #[test]
    fn locks_per_service_lifecycle_and_clock_schedule() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      faults:\n        - at_round: 2\n          kind: pause\n          duration_rounds: 3\n        - at_round: 6\n          kind: restart\n        - at_round: 8\n          kind: clock_jump\n          nanoseconds: 1000000000\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.services["api"].faults.len(), 3);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["services"]["api"]["faults"][2]["kind"], "clock_jump");
    }

    #[test]
    fn rejects_clock_jump_without_virtual_time() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      faults:\n        - at_round: 1\n          kind: clock_jump\n          nanoseconds: 1\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let manifest = directory.path().join("api/theseus.toml");
        let input = fs::read_to_string(&manifest).unwrap();
        fs::write(
            manifest,
            input.replace(
                "[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
                "",
            ),
        )
        .unwrap();
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("clock_jump requires virtual_time")
        );
    }
}
