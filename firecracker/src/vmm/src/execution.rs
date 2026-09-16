// Copyright 2026 Adrian Mârza and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Portable, bounded machine-stream evidence for API-driven VMs.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::vstate::vcpu::{ExecutionLedgerEvidence, MachineExecutionController};
use crate::{Vmm, VmmError};

/// Pre-boot execution capture and optional active replay configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    /// New output file; an existing file is never overwritten.
    pub evidence_path: PathBuf,
    /// JSON array of exact decisions to admit, installed before the first vCPU run.
    pub replay_trace_path: Option<PathBuf>,
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
}

impl ExecutionConfig {
    pub(crate) fn prepare(&self) -> Result<(File, Option<Vec<String>>), VmmError> {
        // Read and validate before creating output or starting vCPU threads.
        let expected = self
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
            .transpose()?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.evidence_path)
            .map_err(|error| {
                VmmError::ExecutionCoverage(format!(
                    "create {}: {error}",
                    self.evidence_path.display()
                ))
            })?;
        Ok((file, expected))
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
                "execution capture was not configured before boot".into(),
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
        };
        let file = self
            .execution_evidence_file
            .as_mut()
            .expect("configured evidence file");
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|error| {
                VmmError::ExecutionCoverage(format!("rewind execution evidence: {error}"))
            })?;
        serde_json::to_writer(&mut *file, &evidence).map_err(|error| {
            VmmError::ExecutionCoverage(format!("serialize execution evidence: {error}"))
        })?;
        file.write_all(b"\n")
            .and_then(|()| file.flush())
            .map_err(|error| {
                VmmError::ExecutionCoverage(format!("write execution evidence: {error}"))
            })
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
        };
        assert!(config.prepare().is_err());
        assert!(!output.exists());
    }
}
