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
    InvalidNetworkDuplicateRate(u32),
    InvalidRunConfig(String),
    InvalidExplore(String),
    InvalidStorage(String),
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
            Self::InvalidNetworkDuplicateRate(rate) => {
                write!(
                    formatter,
                    "network.duplicate_ppm must be at most 1000000, got {rate}"
                )
            }
            Self::InvalidRunConfig(reason) => write!(formatter, "run: {reason}"),
            Self::InvalidExplore(reason) => write!(formatter, "explore: {reason}"),
            Self::InvalidStorage(reason) => write!(formatter, "storage: {reason}"),
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
    storage: Vec<Storage>,
    #[serde(default)]
    explore: Option<Explore>,
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
    duplicate_ppm: u32,
    #[serde(default)]
    partitioned: bool,
    #[serde(default)]
    latency_rounds: u32,
    #[serde(default)]
    jitter_rounds: u32,
    #[serde(default)]
    tx_bytes_per_round: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Storage {
    id: String,
    size_mib: u32,
    #[serde(default)]
    error_ppm: u32,
    #[serde(default)]
    latency_rounds: u32,
    #[serde(default)]
    torn_write_bytes: Option<u32>,
    #[serde(default)]
    corrupt_read_xor: Option<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Explore {
    max_nodes: u32,
    branches_per_node: u32,
    max_depth: u32,
    #[serde(default = "default_explore_run_ms")]
    run_ms: u64,
    #[serde(default)]
    rendezvous: bool,
    #[serde(default)]
    branch_event_suffix: bool,
    #[serde(default)]
    novelty: Novelty,
    #[serde(default)]
    events: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Novelty {
    #[default]
    Markers,
    Coverage,
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
    MarkerNotSeen,
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
    pub storage: Vec<StoragePlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explore: Option<ExplorePlan>,
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
    /// The Linux exploration executor published beside the CLI. It is added
    /// by `theseus explore`, not by a user manifest, then locked into an
    /// exploration bundle so replay does not silently change executors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explorer_runner: Option<ArtifactPlan>,
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
    #[serde(default)]
    pub duplicate_ppm: u32,
    pub partitioned: bool,
    #[serde(default)]
    pub latency_rounds: u32,
    #[serde(default)]
    pub jitter_rounds: u32,
    #[serde(default)]
    pub tx_bytes_per_round: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StoragePlan {
    pub id: String,
    pub size_mib: u32,
    pub seed: u64,
    pub error_ppm: u32,
    pub latency_rounds: u32,
    pub torn_write_bytes: Option<u32>,
    pub corrupt_read_xor: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExplorePlan {
    pub max_nodes: u32,
    pub branches_per_node: u32,
    pub max_depth: u32,
    pub run_ms: u64,
    pub rendezvous: bool,
    pub branch_event_suffix: bool,
    pub novelty: Novelty,
    pub events_hex: Vec<String>,
    /// A root-to-node seed path injected only by `theseus explore --replay`.
    /// User manifests cannot set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_seed_path: Option<Vec<u64>>,
    /// Fingerprint recorded for a targeted replay. Injected from the locked
    /// result; user manifests cannot set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_expected: Option<ReplayFingerprint>,
    /// Fingerprints for every recorded timeline. Injected only for a whole
    /// exploration replay from its locked result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_expected_tree: Option<Vec<ReplayTreeNode>>,
}

/// Guest-visible fingerprints that a targeted replay must reproduce.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplayFingerprint {
    pub entropy_probe_hex: String,
    pub markers_hex: String,
    pub dirty_pages: Option<u64>,
    /// Digest of the captured serial console, when the source exploration
    /// bundle recorded one. Older bundles omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_sha256: Option<String>,
}

/// The expected fingerprint of one seed path during a whole-tree replay.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplayTreeNode {
    pub seed_path: Vec<u64>,
    pub fingerprint: ReplayFingerprint,
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
    if manifest.network.duplicate_ppm > 1_000_000 {
        return Err(LoadError::InvalidNetworkDuplicateRate(
            manifest.network.duplicate_ppm,
        ));
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
    let storage = storage_plan(manifest.storage, manifest.run.seed)?;
    let explore = explore_plan(manifest.explore)?;

    Ok(RunPlan {
        format: "theseus-run-plan-v1".to_owned(),
        manifest: manifest_path.display().to_string(),
        runtime: RuntimePlan {
            firecracker,
            explorer_runner: None,
        },
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
            duplicate_ppm: manifest.network.duplicate_ppm,
            partitioned: manifest.network.partitioned,
            latency_rounds: manifest.network.latency_rounds,
            jitter_rounds: manifest.network.jitter_rounds,
            tx_bytes_per_round: manifest.network.tx_bytes_per_round,
        },
        storage,
        explore,
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

fn explore_plan(explore: Option<Explore>) -> Result<Option<ExplorePlan>, LoadError> {
    const MAX_NODES: u32 = 1024;
    let Some(explore) = explore else {
        return Ok(None);
    };
    if explore.max_nodes == 0 || explore.max_nodes > MAX_NODES {
        return Err(LoadError::InvalidExplore(format!(
            "explore.max_nodes must be between 1 and {MAX_NODES}"
        )));
    }
    if explore.branches_per_node == 0 {
        return Err(LoadError::InvalidExplore(
            "explore.branches_per_node must be greater than zero".to_owned(),
        ));
    }
    if explore.run_ms == 0 {
        return Err(LoadError::InvalidExplore(
            "explore.run_ms must be greater than zero".to_owned(),
        ));
    }
    if !explore.rendezvous {
        return Err(LoadError::InvalidExplore(
            "rendezvous must be true; host-time exploration is not replayable".to_owned(),
        ));
    }
    let events_hex = explore
        .events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let bytes = decode_hex(event).map_err(|reason| {
                LoadError::InvalidExplore(format!("events[{index}]: {reason}"))
            })?;
            if bytes.len() != 1 || bytes[0] == 0 {
                return Err(LoadError::InvalidExplore(format!(
                    "events[{index}] must be one non-zero byte"
                )));
            }
            Ok(hex(bytes))
        })
        .collect::<Result<_, LoadError>>()?;
    Ok(Some(ExplorePlan {
        max_nodes: explore.max_nodes,
        branches_per_node: explore.branches_per_node,
        max_depth: explore.max_depth,
        run_ms: explore.run_ms,
        rendezvous: explore.rendezvous,
        branch_event_suffix: explore.branch_event_suffix,
        novelty: explore.novelty,
        events_hex,
        replay_seed_path: None,
        replay_expected: None,
        replay_expected_tree: None,
    }))
}

fn storage_plan(storage: Vec<Storage>, run_seed: u64) -> Result<Vec<StoragePlan>, LoadError> {
    const MAX_STORAGE_MIB: u32 = 1024;

    let mut ids = HashSet::new();
    storage
        .into_iter()
        .enumerate()
        .map(|(index, storage)| {
            if storage.id.is_empty()
                || !storage
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            {
                return Err(LoadError::InvalidStorage(format!(
                    "id {:?} must contain only letters, digits, '-' or '_'",
                    storage.id
                )));
            }
            if !ids.insert(storage.id.clone()) {
                return Err(LoadError::InvalidStorage(format!(
                    "id {:?} appears more than once",
                    storage.id
                )));
            }
            if storage.size_mib == 0 || storage.size_mib > MAX_STORAGE_MIB {
                return Err(LoadError::InvalidStorage(format!(
                    "size_mib for {:?} must be between 1 and {MAX_STORAGE_MIB}",
                    storage.id
                )));
            }
            if storage.error_ppm > 1_000_000 {
                return Err(LoadError::InvalidStorage(format!(
                    "error_ppm for {:?} must be at most 1000000",
                    storage.id
                )));
            }
            if storage.torn_write_bytes == Some(0) {
                return Err(LoadError::InvalidStorage(format!(
                    "torn_write_bytes for {:?} must be greater than zero",
                    storage.id
                )));
            }
            if storage.corrupt_read_xor == Some(0) {
                return Err(LoadError::InvalidStorage(format!(
                    "corrupt_read_xor for {:?} must not be zero",
                    storage.id
                )));
            }

            let mut digest = Sha256::new();
            digest.update(run_seed.to_le_bytes());
            digest.update((index as u64).to_le_bytes());
            digest.update(storage.id.as_bytes());
            let seed = u64::from_le_bytes(digest.finalize()[..8].try_into().unwrap());
            Ok(StoragePlan {
                id: storage.id,
                size_mib: storage.size_mib,
                seed,
                error_ppm: storage.error_ppm,
                latency_rounds: storage.latency_rounds,
                torn_write_bytes: storage.torn_write_bytes,
                corrupt_read_xor: storage.corrupt_read_xor,
            })
        })
        .collect()
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_explore_run_ms() -> u64 {
    100
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
duplicate_ppm = 200
partitioned = false
latency_rounds = 2
jitter_rounds = 1
tx_bytes_per_round = 512

[[storage]]
id = "data_1"
size_mib = 4
error_ppm = 250
latency_rounds = 2
torn_write_bytes = 16
corrupt_read_xor = 1
"#,
        );

        let plan = load_plan(directory.path().join("test/theseus.toml")).unwrap();
        assert_eq!(plan.format, "theseus-run-plan-v1");
        assert_eq!(plan.run.seed, 42);
        assert_eq!(plan.events[0].data_hex, "aa00");
        assert_eq!(plan.network.drop_ppm, 100);
        assert_eq!(plan.network.duplicate_ppm, 200);
        assert_eq!(plan.network.latency_rounds, 2);
        assert_eq!(plan.network.jitter_rounds, 1);
        assert_eq!(plan.network.tx_bytes_per_round, 512);
        assert_eq!(plan.storage.len(), 1);
        assert_eq!(plan.storage[0].id, "data_1");
        assert_eq!(plan.storage[0].torn_write_bytes, Some(16));
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

    #[test]
    fn rejects_invalid_simulated_storage() {
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
[[storage]]
id = "data"
size_mib = 0
"#,
        );
        let error = load_plan(directory.path().join("test/theseus.toml")).unwrap_err();
        assert!(error.to_string().contains("size_mib"));
    }

    #[test]
    fn normalizes_a_bounded_rendezvous_exploration_contract() {
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
[explore]
max_nodes = 7
branches_per_node = 2
max_depth = 2
rendezvous = true
branch_event_suffix = true
novelty = "coverage"
events = ["90", "0a"]
"#,
        );
        let plan = load_plan(directory.path().join("test/theseus.toml")).unwrap();
        let explore = plan.explore.unwrap();
        assert_eq!(explore.max_nodes, 7);
        assert_eq!(explore.events_hex, ["90", "0a"]);
        assert!(matches!(explore.novelty, Novelty::Coverage));
        assert!(explore.replay_seed_path.is_none());
    }
}
