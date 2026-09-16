// Copyright 2026 Adrian Mârza and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Immutable paused VM checkpoints. Their inherited prefix is not boot replay.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution::ExecutionConfig;
use crate::persist::{create_snapshot, VmInfo};
use crate::vmm_config::entropy::EntropyDeviceConfig;
use crate::vmm_config::machine_config::MachineConfig;
use crate::vmm_config::snapshot::{CreateSnapshotParams, SnapshotType};
use crate::vstate::vcpu::{CheckpointExecutionState, ExecutionLedger, MachineExecutionState};
use crate::{Vmm, VmmError};
use theseus_engine::door::ControlState;

const MAX_METADATA_BYTES: u64 = 128 * 1024 * 1024;

fn failure(reason: impl std::fmt::Display) -> VmmError {
    VmmError::ExecutionCoverage(format!("execution checkpoint: {reason}"))
}

fn architecture() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        "arm64"
    }
}

/// Capture into a new directory; existing files are never overwritten.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCheckpointConfig {
    /// Newly created checkpoint directory.
    pub directory: PathBuf,
}

/// Load verified state before any resumed execution or active replay.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoadCheckpointConfig {
    /// Directory containing metadata.json, vmstate, and memory.
    pub directory: PathBuf,
    /// Locked metadata digest; metadata in turn binds VM state and RAM.
    pub checkpoint_sha256: String,
    /// New evidence output and optional full expected stream.
    pub execution: ExecutionConfig,
    /// New UART output, independent of the original capture path.
    pub serial_out_path: PathBuf,
}

/// Retained byte length and SHA-256 of one fixed-name checkpoint member.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointArtifact {
    /// Exact byte length.
    pub bytes: u64,
    /// SHA-256 of all file bytes.
    pub sha256: String,
}

/// PS/2 registers and logical FIFO; host eventfds are attached afresh.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardState {
    /// Controller status register.
    pub status: u8,
    /// Controller command byte.
    pub control: u8,
    /// Output port register.
    pub outp: u8,
    /// Pending command.
    pub cmd: u8,
    /// Logical FIFO in guest read order.
    pub buffer: Vec<u8>,
}

/// Commit marker written only after state, RAM, and runtime context exist.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointMetadata {
    /// `theseus-checkpoint-v1`.
    pub format: String,
    /// Native runtime architecture; snapshots are not cross-architecture.
    pub architecture: String,
    /// Hash/length of the standard Firecracker snapshot.
    pub snapshot: CheckpointArtifact,
    /// Hash/length of the full contiguous guest RAM dump.
    pub memory: CheckpointArtifact,
    /// Captured machine and exit-counted clock configuration.
    pub machine_config: MachineConfig,
    /// Original seed configuration, not a request to reseed restored entropy.
    pub entropy: Option<EntropyDeviceConfig>,
    /// Prefix and ordered undelivered userspace interrupts.
    pub execution: CheckpointExecutionState,
    /// Transient control-channel state absent from standard snapshots.
    pub control: ControlState,
    /// PS/2 state on amd64; absent on arm64.
    pub(crate) keyboard: Option<KeyboardState>,
}

pub(crate) struct VerifiedCheckpoint {
    pub metadata: CheckpointMetadata,
    pub machine: MachineExecutionState,
    pub ledgers: Vec<ExecutionLedger>,
}

/// Transient emulated device state shared by API and topology checkpoints.
/// Host eventfds and replay controllers are deliberately attached afresh.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionDeviceState {
    /// Control FIFO and guest command history.
    pub control: ControlState,
    /// PS/2 registers/FIFO on amd64; absent on arm64.
    pub keyboard: Option<KeyboardState>,
}

impl ExecutionDeviceState {
    /// Validate architecture and bounded queues before creating restored VMs.
    pub fn validate(&self) -> Result<(), VmmError> {
        if self.keyboard.is_some() != cfg!(target_arch = "x86_64")
            || self
                .keyboard
                .as_ref()
                .is_some_and(|keyboard| keyboard.buffer.len() > 16)
            || self.control.host_events.len() > 1_048_576
            || self.control.event_log.len() > 1_048_576
        {
            return Err(failure("invalid transient device state"));
        }
        Ok(())
    }
}

/// Hash a bounded, regular checkpoint member without loading RAM into memory.
pub fn artifact(path: &Path, maximum: u64) -> Result<CheckpointArtifact, VmmError> {
    let metadata = fs::symlink_metadata(path).map_err(failure)?;
    if !metadata.file_type().is_file() || metadata.len() > maximum {
        return Err(failure(
            "member must be a bounded regular file, not a symlink",
        ));
    }
    let mut input = BufReader::new(File::open(path).map_err(failure)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let count = input.read(&mut buffer).map_err(failure)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or_else(|| failure("member size overflow"))?;
        if bytes > maximum {
            return Err(failure("member exceeds size bound"));
        }
        hasher.update(&buffer[..count]);
    }
    if bytes != metadata.len() {
        return Err(failure("member changed while hashing"));
    }
    Ok(CheckpointArtifact {
        bytes,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

impl LoadCheckpointConfig {
    pub(crate) fn verify(&self) -> Result<VerifiedCheckpoint, VmmError> {
        if !fs::symlink_metadata(&self.directory)
            .map_err(failure)?
            .file_type()
            .is_dir()
        {
            return Err(failure("checkpoint directory must not be a symlink"));
        }
        let metadata_path = self.directory.join("metadata.json");
        if artifact(&metadata_path, MAX_METADATA_BYTES)?.sha256 != self.checkpoint_sha256 {
            return Err(failure("metadata digest differs from its locked identity"));
        }
        let mut bytes = Vec::new();
        File::open(&metadata_path)
            .and_then(|file| file.take(MAX_METADATA_BYTES + 1).read_to_end(&mut bytes))
            .map_err(failure)?;
        if bytes.len() as u64 > MAX_METADATA_BYTES
            || format!("{:x}", Sha256::digest(&bytes)) != self.checkpoint_sha256
        {
            return Err(failure(
                "metadata exceeded its bound or changed after hashing",
            ));
        }
        let metadata: CheckpointMetadata = serde_json::from_slice(&bytes).map_err(failure)?;
        if metadata.format != "theseus-checkpoint-v1" || metadata.architecture != architecture() {
            return Err(failure("unsupported format or native architecture"));
        }
        let config = &metadata.machine_config;
        if config.vcpu_count == 0
            || config.vcpu_count > 32
            || config.mem_size_mib == 0
            || config.mem_size_mib > 64 * 1024
            || config.virtual_time.is_none()
            || config
                .virtual_time
                .is_some_and(|time| time.tick_ns == 0 || time.exits_per_tick == 0)
            || metadata.control.host_events.len() > 1_048_576
            || metadata.control.event_log.len() > 1_048_576
            || metadata.keyboard.is_some() != cfg!(target_arch = "x86_64")
            || metadata
                .keyboard
                .as_ref()
                .is_some_and(|state| state.buffer.len() > 16)
            || metadata
                .entropy
                .as_ref()
                .is_some_and(|entropy| entropy.rate_limiter.is_some())
        {
            return Err(failure(
                "invalid machine, clock, control, keyboard, or entropy state",
            ));
        }
        let maximum_memory = (config.mem_size_mib as u64) * 1024 * 1024;
        for (name, expected, maximum) in [
            ("vmstate", &metadata.snapshot, MAX_METADATA_BYTES),
            ("memory", &metadata.memory, maximum_memory),
        ] {
            let actual = artifact(&self.directory.join(name), maximum)?;
            if actual.bytes != expected.bytes || actual.sha256 != expected.sha256 {
                return Err(failure(format!(
                    "{name} differs from the retained checkpoint"
                )));
            }
        }
        if metadata.memory.bytes != maximum_memory {
            return Err(failure(
                "RAM length differs from captured memory configuration",
            ));
        }
        let (machine, ledgers) = metadata
            .execution
            .restore(usize::from(config.vcpu_count))
            .map_err(failure)?;
        Ok(VerifiedCheckpoint {
            metadata,
            machine,
            ledgers,
        })
    }
}

impl Vmm {
    /// Capture transient devices after the standard snapshot has been saved.
    pub fn execution_device_state(&self) -> Result<ExecutionDeviceState, VmmError> {
        let control = self
            .device_manager
            .mmio_platform_devices
            .theseus
            .as_ref()
            .ok_or_else(|| failure("missing control device"))?
            .inner
            .lock()
            .expect("Poisoned lock")
            .checkpoint_state();
        #[cfg(target_arch = "x86_64")]
        let keyboard = Some(
            self.device_manager
                .legacy_devices
                .as_ref()
                .ok_or_else(|| failure("missing keyboard device"))?
                .i8042
                .lock()
                .expect("Poisoned lock")
                .checkpoint_state(),
        );
        #[cfg(target_arch = "aarch64")]
        let keyboard = None;
        Ok(ExecutionDeviceState { control, keyboard })
    }

    /// Restore logical device state without replaying commands or notifications.
    pub fn restore_execution_devices(
        &mut self,
        state: &ExecutionDeviceState,
    ) -> Result<(), VmmError> {
        state.validate()?;
        self.device_manager
            .mmio_platform_devices
            .theseus
            .as_ref()
            .ok_or_else(|| failure("missing control device"))?
            .inner
            .lock()
            .expect("Poisoned lock")
            .restore_checkpoint_state(state.control.clone());
        #[cfg(target_arch = "x86_64")]
        self.device_manager
            .legacy_devices
            .as_ref()
            .ok_or_else(|| failure("missing keyboard device"))?
            .i8042
            .lock()
            .expect("Poisoned lock")
            .restore_checkpoint_state(state.keyboard.as_ref().unwrap());
        Ok(())
    }
    /// Capture a paused API-driven UART/RNG guest into a new immutable directory.
    pub fn create_execution_checkpoint(
        &mut self,
        config: &CreateCheckpointConfig,
    ) -> Result<(), VmmError> {
        if self.serial_output_rate_limited {
            return Err(failure(
                "checkpoint capture does not retain UART rate-limiter state",
            ));
        }
        if self.instance_info.state != crate::vmm_config::instance_info::VmState::Paused
            || self.shutdown_exit_code.is_some()
            || self.execution_config.is_none()
            || self.machine_config.virtual_time.is_none()
            || self
                .execution_config
                .as_ref()
                .is_some_and(|config| config.replay_trace_path.is_some())
        {
            return Err(failure("capture requires a paused nonterminal VM with configured execution and virtual time, not active replay"));
        }
        let full = self.full_config();
        if !full.drives.is_empty()
            || !full.network_interfaces.is_empty()
            || full.vsock.is_some()
            || !full.pmem_devices.is_empty()
            || full.balloon.is_some()
            || full.memory_hotplug.is_some()
            || full
                .entropy
                .as_ref()
                .is_some_and(|entropy| entropy.rate_limiter.is_some())
        {
            return Err(failure("API execution checkpoints currently support UART, RNG, and built-in platform/control devices only"));
        }
        fs::create_dir(&config.directory).map_err(failure)?;
        let snapshot = config.directory.join("vmstate");
        let memory = config.directory.join("memory");
        // Device save may enqueue requests. Capture runtime context after it.
        let info = VmInfo {
            mem_size_mib: self.machine_config.mem_size_mib as u64,
            smt: self.machine_config.smt,
            cpu_template: crate::cpu_config::templates::StaticCpuTemplate::from(
                &self.machine_config.cpu_template,
            ),
            boot_source: self.boot_source_config.clone(),
            huge_pages: self.machine_config.huge_pages,
        };
        create_snapshot(
            self,
            &info,
            &CreateSnapshotParams {
                snapshot_type: SnapshotType::Full,
                snapshot_path: snapshot.clone(),
                mem_file_path: memory.clone(),
                sync_snapshot_files: true,
            },
        )
        .map_err(failure)?;
        let control = self
            .device_manager
            .mmio_platform_devices
            .theseus
            .as_ref()
            .ok_or_else(|| failure("missing control device"))?
            .inner
            .lock()
            .expect("Poisoned lock")
            .checkpoint_state();
        #[cfg(target_arch = "x86_64")]
        let keyboard = Some(
            self.device_manager
                .legacy_devices
                .as_ref()
                .ok_or_else(|| failure("missing keyboard device"))?
                .i8042
                .lock()
                .expect("Poisoned lock")
                .checkpoint_state(),
        );
        #[cfg(target_arch = "aarch64")]
        let keyboard = None;
        let metadata = CheckpointMetadata {
            format: "theseus-checkpoint-v1".into(),
            architecture: architecture().into(),
            snapshot: artifact(&snapshot, MAX_METADATA_BYTES)?,
            memory: artifact(
                &memory,
                self.machine_config.mem_size_mib as u64 * 1024 * 1024,
            )?,
            machine_config: self.machine_config.clone(),
            entropy: full.entropy,
            execution: self.machine_execution_state()?.checkpoint_state(),
            control,
            keyboard,
        };
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(config.directory.join("metadata.json"))
            .map_err(failure)?;
        let mut output = BufWriter::new(file);
        serde_json::to_writer(&mut output, &metadata).map_err(failure)?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(failure)?;
        output.get_ref().sync_all().map_err(failure)?;
        Ok(())
    }

    pub(crate) fn restore_checkpoint_devices(
        &mut self,
        metadata: &CheckpointMetadata,
    ) -> Result<(), VmmError> {
        self.device_manager
            .mmio_platform_devices
            .theseus
            .as_ref()
            .ok_or_else(|| failure("missing control device"))?
            .inner
            .lock()
            .expect("Poisoned lock")
            .restore_checkpoint_state(metadata.control.clone());
        #[cfg(target_arch = "x86_64")]
        self.device_manager
            .legacy_devices
            .as_ref()
            .ok_or_else(|| failure("missing keyboard device"))?
            .i8042
            .lock()
            .expect("Poisoned lock")
            .restore_checkpoint_state(
                metadata
                    .keyboard
                    .as_ref()
                    .ok_or_else(|| failure("missing keyboard state"))?,
            );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_sys_util::tempdir::TempDir;

    fn fixture() -> (TempDir, LoadCheckpointConfig) {
        let directory = TempDir::new().unwrap();
        fs::write(directory.as_path().join("vmstate"), b"snapshot bytes").unwrap();
        fs::write(directory.as_path().join("memory"), vec![0; 1024 * 1024]).unwrap();
        let mut machine_config = MachineConfig::default();
        machine_config.mem_size_mib = 1;
        machine_config.virtual_time = Some(crate::vmm_config::machine_config::VirtualTimeConfig {
            tick_ns: 1_000_000,
            exits_per_tick: 10,
        });
        let metadata = CheckpointMetadata {
            format: "theseus-checkpoint-v1".into(),
            architecture: architecture().into(),
            snapshot: artifact(&directory.as_path().join("vmstate"), MAX_METADATA_BYTES).unwrap(),
            memory: artifact(&directory.as_path().join("memory"), 1024 * 1024).unwrap(),
            machine_config,
            entropy: None,
            execution: CheckpointExecutionState {
                trace: vec!["vcpu:0:pio_write:0x3f8:1:41".into()],
                pending_interrupts: vec![],
            },
            control: ControlState::default(),
            keyboard: cfg!(target_arch = "x86_64").then_some(KeyboardState {
                status: 0,
                control: 0,
                outp: 0,
                cmd: 0,
                buffer: vec![],
            }),
        };
        let path = directory.as_path().join("metadata.json");
        fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let config = LoadCheckpointConfig {
            directory: directory.as_path().into(),
            checkpoint_sha256: artifact(&path, MAX_METADATA_BYTES).unwrap().sha256,
            execution: ExecutionConfig {
                evidence_path: directory.as_path().join("evidence.json"),
                replay_trace_path: None,
                start: None,
            },
            serial_out_path: directory.as_path().join("serial.log"),
        };
        (directory, config)
    }

    #[test]
    fn checkpoint_verification_rebuilds_prefix_before_creating_outputs() {
        let (_directory, config) = fixture();
        let verified = config.verify().unwrap();
        assert_eq!(verified.machine.ledger_evidence().decisions, 1);
        assert_eq!(verified.ledgers[0].evidence().decisions, 1);
        assert!(!config.execution.evidence_path.exists());
        assert!(!config.serial_out_path.exists());
    }

    #[test]
    fn checkpoint_rejects_tampered_and_symlinked_members() {
        for name in ["metadata.json", "vmstate", "memory"] {
            let (_directory, config) = fixture();
            fs::write(config.directory.join(name), b"changed").unwrap();
            assert!(config.verify().is_err(), "{name}");
            assert!(!config.execution.evidence_path.exists());
        }
        let (_directory, config) = fixture();
        let memory = config.directory.join("memory");
        fs::rename(&memory, config.directory.join("other")).unwrap();
        std::os::unix::fs::symlink("other", &memory).unwrap();
        assert!(config.verify().is_err());
    }

    #[test]
    fn checkpoint_rejects_a_resealed_wrong_architecture_and_unconfigured_clock() {
        for field in ["architecture", "virtual_time"] {
            let (_directory, mut config) = fixture();
            let path = config.directory.join("metadata.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if field == "architecture" {
                value[field] = "wrong".into();
            } else {
                value["machine_config"][field] = serde_json::Value::Null;
            }
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            config.checkpoint_sha256 = artifact(&path, MAX_METADATA_BYTES).unwrap().sha256;
            assert!(config.verify().is_err());
            assert!(!config.execution.evidence_path.exists());
        }
    }
}
