// Copyright 2026 Adrian Mârza and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Portable, bounded machine-stream evidence for API-driven VMs.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::vstate::vcpu::{
    ExecutionLedgerEvidence, MachineExecutionController, TimerObservation,
};
use crate::{Vmm, VmmError};

/// Pre-boot execution capture and optional active replay configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    /// New output file; an existing file is never overwritten.
    pub evidence_path: PathBuf,
    /// JSON array of exact decisions to admit, installed before the first vCPU run.
    pub replay_trace_path: Option<PathBuf>,
    /// Set internally only after a verified checkpoint load.
    #[serde(skip)]
    pub(crate) start: Option<ExecutionStart>,
}

/// Immutable checkpoint origin; its prefix is inherited, not rerun from boot.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStart {
    /// `checkpoint` for a retained paused guest.
    pub kind: String,
    /// Digest of the checkpoint metadata, which binds state and RAM.
    pub checkpoint_sha256: String,
    /// Number of decisions inherited before the resumed execution.
    pub inherited_decisions: u64,
}

/// Complete machine decisions, local ledgers, and the observed terminal boundary.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvidence {
    /// Evidence schema identity.
    pub format: String,
    /// `guest_exit`, `runtime_error`, or `pause`; only a guest exit is replayable.
    pub boundary: String,
    /// Ordered per-vCPU rolling digests and readable tails.
    pub execution_ledgers: Vec<ExecutionLedgerEvidence>,
    /// Digest and readable tail of the complete machine stream.
    pub machine_execution_ledger: ExecutionLedgerEvidence,
    /// Complete bounded decisions, including host input.
    pub machine_execution_trace: Vec<String>,
    /// First active divergence or an unconsumed expected suffix.
    pub replay_error: Option<String>,
    /// Absent for legacy fresh-boot capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<ExecutionStart>,
}

/// Sibling evidence for in-kernel timer deliveries. They stay outside
/// `execution.json` because they depend on host drift inside a quantum, and
/// replayed bundles must keep byte-identical execution evidence.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimerObservationEvidence {
    /// Evidence schema identity.
    pub format: String,
    /// Ordered in-kernel timer delivery observations.
    pub observations: Vec<TimerObservation>,
}

/// Sibling file written beside one execution capture.
pub const TIMER_OBSERVATIONS_FILE: &str = "timer-observations.json";

/// Serialize the bounded observation list, or an empty list when the
/// deterministic profile observed nothing.
pub(crate) fn write_timer_observation_evidence(
    path: &Path,
    observations: Vec<TimerObservation>,
) -> Result<(), VmmError> {
    let evidence = TimerObservationEvidence {
        format: "theseus-timer-observations-v1".to_owned(),
        observations,
    };
    let bytes = serde_json::to_vec_pretty(&evidence)
        .map_err(|error| VmmError::ExecutionCoverage(format!("serialize timer observations: {error}")))?;
    std::fs::write(path, bytes).map_err(|error| {
        VmmError::ExecutionCoverage(format!("write {}: {error}", path.display()))
    })?;
    Ok(())
}

impl ExecutionConfig {
    pub(crate) fn prepare(&self) -> Result<(File, Option<Vec<String>>), VmmError> {
        let expected = self.read_replay_trace()?;
        Ok((self.create_evidence_file()?, expected))
    }

    pub(crate) fn read_replay_trace(&self) -> Result<Option<Vec<String>>, VmmError> {
        // Read and validate before creating output or starting vCPU threads.
        self
            .replay_trace_path
            .as_ref()
            .map(|path| {
                let mut bytes = Vec::new();
                File::open(path)
                    .and_then(|file| file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes))
                    .map_err(|error| {
                        VmmError::ExecutionCoverage(format!("read {}: {error}", path.display()))
                    })?;
                if bytes.len() > 128 * 1024 * 1024 {
                    return Err(VmmError::ExecutionCoverage(
                        "replay trace exceeds 128 MiB".into(),
                    ));
                }
                let trace: Vec<String> = serde_json::from_slice(&bytes).map_err(|error| {
                    VmmError::ExecutionCoverage(format!("invalid replay trace: {error}"))
                })?;
                MachineExecutionController::validate_trace(&trace)
                    .map_err(VmmError::ExecutionCoverage)?;
                Ok(trace)
            })
            .transpose()
    }

    pub(crate) fn create_evidence_file(&self) -> Result<File, VmmError> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.evidence_path)
            .map_err(|error| {
                VmmError::ExecutionCoverage(format!(
                    "create {}: {error}",
                    self.evidence_path.display()
                ))
            })
    }
}

impl Vmm {
    /// Flush a stable paused or terminal machine boundary to the configured file.
    pub fn flush_execution_evidence(&mut self) -> Result<(), VmmError> {
        if self.instance_info.state != crate::vmm_config::instance_info::VmState::Paused
            && self.shutdown_exit_code.is_none()
        {
            return Err(VmmError::ExecutionCoverage(
                "pause the VM before flushing execution evidence".into(),
            ));
        }
        if self.execution_evidence_file.is_none() {
            return Err(VmmError::ExecutionCoverage(
                "execution capture was not configured for this VM".into(),
            ));
        }
        let evidence = ExecutionEvidence {
            format: "theseus-execution-v1".into(),
            boundary: match self.shutdown_exit_code {
                Some(crate::FcExitCode::Ok) => "guest_exit",
                Some(_) => "runtime_error",
                None => "pause",
            }
            .into(),
            execution_ledgers: self.execution_ledger_evidence()?,
            machine_execution_ledger: self.machine_execution_ledger_evidence()?,
            machine_execution_trace: self.machine_execution_trace()?,
            replay_error: self.machine_execution_replay_error()?,
            start: self.execution_config.as_ref().and_then(|config| config.start.clone()),
        };
        let timer_path = self
            .execution_config
            .as_ref()
            .map(|config| config.evidence_path.with_file_name(TIMER_OBSERVATIONS_FILE));
        let timer_observations = self.machine_timer_observations()?;
        let file = self
            .execution_evidence_file
            .as_mut()
            .expect("configured evidence file");
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|error| {
                VmmError::ExecutionCoverage(format!("rewind execution evidence: {error}"))
            })?;
        // Serialize a large retained stream in buffered writes after admission
        // has stopped, rather than issuing a syscall for each JSON token.
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, &evidence).map_err(|error| {
            VmmError::ExecutionCoverage(format!("serialize execution evidence: {error}"))
        })?;
        writer.write_all(b"\n")
            .and_then(|()| writer.flush())
            .map_err(|error| {
                VmmError::ExecutionCoverage(format!("write execution evidence: {error}"))
            })?;
        if let Some(timer_path) = timer_path {
            write_timer_observation_evidence(&timer_path, timer_observations)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod execution_ledger_tests {
    use super::*;
    use vmm_sys_util::tempfile::TempFile;

    #[test]
    fn capture_does_not_overwrite_an_existing_file() {
        let file = TempFile::new().unwrap();
        let config = ExecutionConfig {
            evidence_path: file.as_path().to_path_buf(),
            replay_trace_path: None,
            start: None,
        };
        assert!(config.prepare().unwrap_err().to_string().contains("create"));
    }

    #[test]
    fn malformed_replay_is_rejected_before_creating_evidence() {
        let input = TempFile::new().unwrap();
        std::fs::write(input.as_path(), b"[\"host:unknown\"]").unwrap();
        let output = input.as_path().with_extension("evidence");
        let config = ExecutionConfig {
            evidence_path: output.clone(),
            replay_trace_path: Some(input.as_path().to_path_buf()),
            start: None,
        };
        assert!(config.prepare().is_err());
        assert!(!output.exists());
    }

    #[test]
    fn timer_observations_round_trip_and_allow_rewrites() {
        let path = TempFile::new().unwrap().as_path().to_path_buf();
        write_timer_observation_evidence(
            &path,
            vec![TimerObservation {
                record: "vcpu:0:timer:vtimer".into(),
                virtual_time_ns: Some(2_000_000),
            }],
        )
        .unwrap();
        let parsed: TimerObservationEvidence =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed.format, "theseus-timer-observations-v1");
        assert_eq!(
            parsed.observations,
            vec![TimerObservation {
                record: "vcpu:0:timer:vtimer".into(),
                virtual_time_ns: Some(2_000_000),
            }]
        );

        // Every flush rewrites the complete list, including an empty one.
        write_timer_observation_evidence(&path, Vec::new()).unwrap();
        let parsed: TimerObservationEvidence =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed.format, "theseus-timer-observations-v1");
        assert!(parsed.observations.is_empty());
    }
}
