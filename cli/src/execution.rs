// Copyright 2026 Adrian Mârza and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Validate portable machine evidence independently of a Linux/KVM runtime.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runner::RunError;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ledger {
    decisions: u64,
    sha256: String,
    tail: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Evidence {
    pub format: String,
    pub boundary: String,
    pub execution_ledgers: Vec<Ledger>,
    pub machine_execution_ledger: Ledger,
    pub machine_execution_trace: Vec<String>,
    pub replay_error: Option<String>,
}

impl Evidence {
    pub fn read(path: &Path, vcpu_count: u8) -> Result<Self, RunError> {
        let invalid = |reason: String| RunError::InvalidBundle {
            path: path.to_path_buf(),
            reason,
        };
        let metadata = std::fs::symlink_metadata(path).map_err(|source| RunError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.file_type().is_file() {
            return Err(invalid(
                "execution evidence must be a regular bundle-local file, not a symlink".into(),
            ));
        }
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|source| RunError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        if bytes.len() > 128 * 1024 * 1024 {
            return Err(invalid("execution evidence exceeds 128 MiB".into()));
        }
        let evidence: Self = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(format!("invalid execution evidence: {error}")))?;
        evidence.validate(vcpu_count).map_err(invalid)?;
        Ok(evidence)
    }

    pub(crate) fn validate(&self, vcpu_count: u8) -> Result<(), String> {
        let trace = &self.machine_execution_trace;
        if self.format != "theseus-execution-v1"
            || !matches!(
                self.boundary.as_str(),
                "guest_exit" | "pause" | "runtime_error"
            )
        {
            return Err("unsupported execution evidence format or boundary".into());
        }
        if trace.len() > 1_048_576
            || self.execution_ledgers.len() != usize::from(vcpu_count)
            || vcpu_count == 0
        {
            return Err("execution evidence has an invalid decision or vCPU count".into());
        }
        let mut local = vec![Vec::new(); usize::from(vcpu_count)];
        for record in trace {
            if !crate::evidence::valid_machine_execution_record(record) {
                return Err("execution evidence contains a malformed decision".into());
            }
            if let Some(record) = record.strip_prefix("vcpu:") {
                let (id, decision) = record.split_once(':').expect("validated actor");
                let id: usize = id.parse().expect("validated actor id");
                let Some(decisions) = local.get_mut(id) else {
                    return Err("execution evidence references an unconfigured vCPU".into());
                };
                decisions.push(decision);
            }
        }
        if self.machine_execution_ledger != ledger(trace.iter().map(String::as_str))
            || self
                .execution_ledgers
                .iter()
                .zip(local)
                .any(|(actual, decisions)| *actual != ledger(decisions.into_iter()))
        {
            return Err(
                "execution evidence digest, count, or tail does not match its complete trace"
                    .into(),
            );
        }
        Ok(())
    }

    pub fn require_replayable(&self, path: &Path) -> Result<(), RunError> {
        let reason = if self.boundary == "pause" {
            Some("a host-timed pause has no actively replayable terminal boundary")
        } else if self.boundary == "runtime_error" {
            Some("a runtime error has no replayable guest terminal boundary")
        } else if self.replay_error.is_some() {
            Some("the original execution already diverged from its replay protocol")
        } else if self.machine_execution_trace.is_empty() {
            Some("the original execution retained no machine decisions")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: reason.into(),
            });
        }
        Ok(())
    }
}

fn ledger<'a>(decisions: impl Iterator<Item = &'a str>) -> Ledger {
    let mut hasher = Sha256::new();
    let mut tail = std::collections::VecDeque::new();
    let mut count = 0;
    for decision in decisions {
        hasher.update((decision.len() as u64).to_le_bytes());
        hasher.update(decision.as_bytes());
        count += 1;
        if tail.len() == 32 {
            tail.pop_front();
        }
        tail.push_back(decision.to_owned());
    }
    Ledger {
        decisions: count,
        sha256: format!("{:x}", hasher.finalize()),
        tail: tail.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> Evidence {
        let trace = vec![
            "host:serial_input:2:2a0a".to_owned(),
            "vcpu:0:mmio_write:0x10:1:2a".to_owned(),
        ];
        Evidence {
            format: "theseus-execution-v1".into(),
            boundary: "guest_exit".into(),
            execution_ledgers: vec![ledger(["mmio_write:0x10:1:2a"].into_iter())],
            machine_execution_ledger: ledger(trace.iter().map(String::as_str)),
            machine_execution_trace: trace,
            replay_error: None,
        }
    }

    #[test]
    fn complete_trace_binds_machine_and_local_ledgers() {
        let mut evidence = evidence();
        evidence.validate(1).unwrap();
        evidence.machine_execution_trace.reverse();
        assert!(evidence.validate(1).unwrap_err().contains("digest"));
    }

    #[test]
    fn incomplete_trace_or_forged_local_digest_is_rejected() {
        let mut evidence = evidence();
        evidence.machine_execution_trace.pop();
        assert!(evidence.validate(1).is_err());
        let mut evidence = super::tests::evidence();
        evidence.execution_ledgers[0].sha256 = "0".repeat(64);
        assert!(evidence.validate(1).is_err());
    }

    #[test]
    fn host_cutoffs_and_prior_divergences_are_not_claimed_as_replay() {
        let mut evidence = evidence();
        evidence.boundary = "pause".into();
        assert!(evidence
            .require_replayable(Path::new("execution.json"))
            .unwrap_err()
            .to_string()
            .contains("host-timed"));
        evidence.boundary = "guest_exit".into();
        evidence.replay_error = Some("missing suffix".into());
        assert!(evidence
            .require_replayable(Path::new("execution.json"))
            .is_err());
    }
}
