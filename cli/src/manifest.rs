// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub enum LoadError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    UnsupportedVersion(u32),
    InvalidEventData {
        index: usize,
        reason: String,
    },
    InvalidNetworkDropRate(u32),
    InvalidRunConfig(String),
    InvalidCheck(String),
    InvalidPath {
        field: &'static str,
        reason: String,
    },
    FileMetadata {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "cannot parse {}: {source}", path.display())
            }
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "manifest version {version} is unsupported (expected {SCHEMA_VERSION})"
            ),
            Self::InvalidEventData { index, reason } => {
                write!(
                    formatter,
                    "events[{index}].data must be lowercase or uppercase hex: {reason}"
                )
            }
            Self::InvalidNetworkDropRate(rate) => {
                write!(
                    formatter,
                    "network.drop_ppm must be at most 1000000, got {rate}"
                )
            }
            Self::InvalidRunConfig(reason) => write!(formatter, "run: {reason}"),
            Self::InvalidCheck(reason) => write!(formatter, "checks: {reason}"),
            Self::InvalidPath { field, reason } => write!(formatter, "{field}: {reason}"),
            Self::FileMetadata { path, source } => {
                write!(formatter, "cannot inspect {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for LoadError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    runtime: Runtime,
    guest: Guest,
    run: Run,
    #[serde(default)]
    events: Vec<Event>,
    #[serde(default)]
    network: Network,
    #[serde(default)]
    checks: Vec<Check>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Runtime {
    firecracker: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Guest {
    kernel: PathBuf,
    initramfs: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    seed: u64,
    vcpu_count: u8,
    mem_size_mib: u32,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default)]
    virtual_time: Option<VirtualTime>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualTime {
    pub tick_ns: u64,
    pub exits_per_tick: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    when: EventWhen,
    data: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventWhen {
    Ready,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Network {
    #[serde(default)]
    loopback: bool,
    #[serde(default)]
    drop_ppm: u32,
    #[serde(default)]
    partitioned: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    name: String,
    kind: CheckKind,
    value: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    SerialContains,
    SerialNotContains,
    MarkerSeen,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunPlan {
    pub format: String,
    pub manifest: String,
    pub runtime: RuntimePlan,
    pub guest: GuestPlan,
    pub run: RunPlanConfig,
    pub events: Vec<EventPlan>,
    pub network: NetworkPlan,
    #[serde(default)]
    pub checks: Vec<CheckPlan>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtifactPlan {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuntimePlan {
    pub firecracker: ArtifactPlan,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GuestPlan {
    pub kernel: ArtifactPlan,
    pub initramfs: ArtifactPlan,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunPlanConfig {
    pub seed: u64,
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    pub timeout_secs: u64,
    pub virtual_time: Option<VirtualTime>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventPlan {
    pub when: EventWhen,
    pub data_hex: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkPlan {
    pub loopback: bool,
    pub drop_ppm: u32,
    pub partitioned: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CheckPlan {
    pub name: String,
    pub kind: CheckKind,
    pub value: String,
}

pub fn load_plan(path: impl AsRef<Path>) -> Result<RunPlan, LoadError> {
    let path = path.as_ref();
    let manifest_path = fs::canonicalize(path).map_err(|source| LoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let manifest_dir = manifest_path
        .parent()
        .expect("a canonical manifest path always has a parent");
    let input = fs::read_to_string(&manifest_path).map_err(|source| LoadError::Read {
        path: manifest_path.clone(),
        source,
    })?;
    let manifest: Manifest = toml::from_str(&input).map_err(|source| LoadError::Parse {
        path: manifest_path.clone(),
        source,
    })?;

    if manifest.version != SCHEMA_VERSION {
        return Err(LoadError::UnsupportedVersion(manifest.version));
    }
    if manifest.network.drop_ppm > 1_000_000 {
        return Err(LoadError::InvalidNetworkDropRate(manifest.network.drop_ppm));
    }
    if manifest.run.vcpu_count == 0 {
        return Err(LoadError::InvalidRunConfig(
            "vcpu_count must be greater than zero".to_owned(),
        ));
    }
    if manifest.run.mem_size_mib == 0 {
        return Err(LoadError::InvalidRunConfig(
            "mem_size_mib must be greater than zero".to_owned(),
        ));
    }
    if manifest.run.timeout_secs == 0 {
        return Err(LoadError::InvalidRunConfig(
            "timeout_secs must be greater than zero".to_owned(),
        ));
    }
    if let Some(virtual_time) = &manifest.run.virtual_time {
        if virtual_time.tick_ns == 0 || virtual_time.exits_per_tick == 0 {
            return Err(LoadError::InvalidRunConfig(
                "virtual_time.tick_ns and virtual_time.exits_per_tick must be greater than zero"
                    .to_owned(),
            ));
        }
    }

    let mut check_names = HashSet::new();
    for check in &manifest.checks {
        if check.name.trim().is_empty() {
            return Err(LoadError::InvalidCheck("name must not be empty".to_owned()));
        }
        if check.name == "guest_exit" || check.name == "completion" {
            return Err(LoadError::InvalidCheck(format!(
                "name {:?} is reserved for a built-in check",
                check.name
            )));
        }
        if !check_names.insert(&check.name) {
            return Err(LoadError::InvalidCheck(format!(
                "name {:?} appears more than once",
                check.name
            )));
        }
        if check.value.is_empty() {
            return Err(LoadError::InvalidCheck(format!(
                "value for {:?} must not be empty",
                check.name
            )));
        }
    }

    let firecracker = artifact(
        manifest_dir,
        "runtime.firecracker",
        &manifest.runtime.firecracker,
    )?;
    ensure_executable("runtime.firecracker", Path::new(&firecracker.path))?;
    let kernel = artifact(manifest_dir, "guest.kernel", &manifest.guest.kernel)?;
    let initramfs = artifact(manifest_dir, "guest.initramfs", &manifest.guest.initramfs)?;

    let events = manifest
        .events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let data = decode_hex(&event.data)
                .map_err(|reason| LoadError::InvalidEventData { index, reason })?;
            Ok(EventPlan {
                when: event.when.clone(),
                data_hex: hex(&data),
            })
        })
        .collect::<Result<_, LoadError>>()?;

    Ok(RunPlan {
        format: "theseus-run-plan-v1".to_owned(),
        manifest: manifest_path.display().to_string(),
        runtime: RuntimePlan { firecracker },
        guest: GuestPlan { kernel, initramfs },
        run: RunPlanConfig {
            seed: manifest.run.seed,
            vcpu_count: manifest.run.vcpu_count,
            mem_size_mib: manifest.run.mem_size_mib,
            timeout_secs: manifest.run.timeout_secs,
            virtual_time: manifest.run.virtual_time,
        },
        events,
        network: NetworkPlan {
            loopback: manifest.network.loopback,
            drop_ppm: manifest.network.drop_ppm,
            partitioned: manifest.network.partitioned,
        },
        checks: manifest
            .checks
            .into_iter()
            .map(|check| CheckPlan {
                name: check.name,
                kind: check.kind,
                value: check.value,
            })
            .collect(),
    })
}

fn default_timeout_secs() -> u64 {
    30
}

fn artifact(dir: &Path, field: &'static str, value: &Path) -> Result<ArtifactPlan, LoadError> {
    if value.is_absolute() {
        return Err(LoadError::InvalidPath {
            field,
            reason: "must be relative to the manifest directory".to_owned(),
        });
    }
    let path = fs::canonicalize(dir.join(value)).map_err(|source| LoadError::FileMetadata {
        path: dir.join(value),
        source,
    })?;
    if !path.starts_with(dir) {
        return Err(LoadError::InvalidPath {
            field,
            reason: "must not escape the manifest directory".to_owned(),
        });
    }
    let metadata = fs::metadata(&path).map_err(|source| LoadError::FileMetadata {
        path: path.clone(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(LoadError::InvalidPath {
            field,
            reason: "must name a regular file".to_owned(),
        });
    }
    let bytes = fs::read(&path).map_err(|source| LoadError::Read {
        path: path.clone(),
        source,
    })?;
    Ok(ArtifactPlan {
        path: path.display().to_string(),
        sha256: hex(Sha256::digest(bytes)),
    })
}

#[cfg(unix)]
fn ensure_executable(field: &'static str, path: &Path) -> Result<(), LoadError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).map_err(|source| LoadError::FileMetadata {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(LoadError::InvalidPath {
            field,
            reason: "must be executable".to_owned(),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_executable(_: &'static str, _: &Path) -> Result<(), LoadError> {
    Ok(())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if value.is_empty() {
        return Err("at least one byte is required".to_owned());
    }
    if !value.len().is_multiple_of(2) {
        return Err("an even number of characters is required".to_owned());
    }
    (0..value.len())
        .step_by(2)
        .map(|offset| {
            u8::from_str_radix(&value[offset..offset + 2], 16)
                .map_err(|_| format!("invalid byte at offset {offset}"))
        })
        .collect()
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
    use std::fs;

    fn fixture(manifest: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let test_directory = directory.path().join("test");
        fs::create_dir_all(test_directory.join("runtime")).unwrap();
        fs::create_dir_all(test_directory.join("guest")).unwrap();
        fs::write(test_directory.join("runtime/firecracker"), b"firecracker").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            test_directory.join("runtime/firecracker"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        fs::write(test_directory.join("guest/vmlinux"), b"kernel").unwrap();
        fs::write(test_directory.join("guest/initramfs.cpio"), b"initramfs").unwrap();
        fs::write(test_directory.join("theseus.toml"), manifest).unwrap();
        directory
    }

    #[test]
    fn resolves_a_complete_manifest_into_a_canonical_plan() {
        let directory = fixture(
            r#"version = 1

[runtime]
firecracker = "runtime/firecracker"

[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs.cpio"

[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128

[run.virtual_time]
tick_ns = 1000000
exits_per_tick = 1024

[[events]]
when = "ready"
data = "Aa00"

[network]
loopback = true
drop_ppm = 100
partitioned = false
"#,
        );

        let plan = load_plan(directory.path().join("test/theseus.toml")).unwrap();
        assert_eq!(plan.format, "theseus-run-plan-v1");
        assert_eq!(plan.run.seed, 42);
        assert_eq!(plan.events[0].data_hex, "aa00");
        assert_eq!(plan.network.drop_ppm, 100);
        assert_eq!(
            plan.runtime.firecracker.sha256,
            "c2d872a13438b3768c94bc023684e6dc78a5fe5fe4c629a9eee8396aa6cba742"
        );
    }

    #[test]
    fn rejects_artifact_paths_that_escape_the_test_directory() {
        let directory = fixture(
            r#"version = 1
[runtime]
firecracker = "../firecracker"
[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs.cpio"
[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128
"#,
        );
        fs::write(directory.path().join("firecracker"), b"outside").unwrap();
        let error = load_plan(directory.path().join("test/theseus.toml")).unwrap_err();
        assert!(error.to_string().contains("must not escape"));
    }

    #[test]
    fn rejects_unknown_manifest_fields() {
        let directory = fixture(
            r#"version = 1
unexpected = true
[runtime]
firecracker = "runtime/firecracker"
[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs.cpio"
[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128
"#,
        );
        let error = load_plan(directory.path().join("test/theseus.toml")).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_invalid_runner_settings_before_kvm_is_needed() {
        let directory = fixture(
            r#"version = 1
[runtime]
firecracker = "runtime/firecracker"
[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs.cpio"
[run]
seed = 42
vcpu_count = 0
mem_size_mib = 128
"#,
        );
        let error = load_plan(directory.path().join("test/theseus.toml")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "run: vcpu_count must be greater than zero"
        );
    }

    #[test]
    fn rejects_duplicate_and_reserved_check_names() {
        let directory = fixture(
            r#"version = 1
[runtime]
firecracker = "runtime/firecracker"
[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs.cpio"
[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128
[[checks]]
name = "guest_exit"
kind = "serial_contains"
value = "done"
"#,
        );
        let error = load_plan(directory.path().join("test/theseus.toml")).unwrap_err();
        assert!(error.to_string().contains("reserved"));
    }
}
