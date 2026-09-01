// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Product boundary for the Linux/KVM exploration executor.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{LoadError, RunPlan, load_plan};

#[derive(Debug)]
pub enum ExploreError {
    Manifest(LoadError),
    Invalid(String),
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    Execute(String),
}

impl fmt::Display for ExploreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => error.fmt(formatter),
            Self::Invalid(reason) => formatter.write_str(reason),
            Self::Write { path, source } => write!(formatter, "cannot write {}: {source}", path.display()),
            Self::Execute(reason) => write!(formatter, "exploration failed: {reason}"),
        }
    }
}

impl std::error::Error for ExploreError {}

/// Start a single-manifest exploration through the executor released beside the CLI.
pub fn explore(path: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<PathBuf, ExploreError> {
    let plan = load_plan(path).map_err(ExploreError::Manifest)?;
    validate(&plan)?;
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ExploreError::Invalid(format!(
            "exploration output already exists: {}",
            output.display()
        )));
    }
    let runner = std::env::current_exe()
        .map_err(|error| ExploreError::Invalid(format!("cannot locate theseus binary: {error}")))?
        .parent()
        .map(|directory| directory.join("theseus-explorer"))
        .ok_or_else(|| ExploreError::Invalid("theseus binary has no parent directory".to_owned()))?;
    if !runner.is_file() {
        return Err(ExploreError::Invalid(format!(
            "missing Linux exploration runner beside theseus: {}; use a published Linux runtime bundle",
            runner.display()
        )));
    }
    let plan_file = output.with_extension("explore-plan.json");
    fs::write(
        &plan_file,
        serde_json::to_vec_pretty(&plan)
            .map_err(|error| ExploreError::Invalid(format!("cannot encode exploration plan: {error}")))?,
    )
    .map_err(|source| ExploreError::Write {
        path: plan_file.clone(),
        source,
    })?;
    let status = Command::new(&runner)
        .arg("--plan")
        .arg(&plan_file)
        .arg("--output")
        .arg(&output)
        .status()
        .map_err(|error| ExploreError::Execute(format!("cannot start {}: {error}", runner.display())))?;
    let _ = fs::remove_file(&plan_file);
    if status.success() {
        Ok(output)
    } else {
        Err(ExploreError::Execute(format!(
            "inspect {} for the locked plan and failure record",
            output.display()
        )))
    }
}

fn validate(plan: &RunPlan) -> Result<(), ExploreError> {
    if plan.explore.is_none() {
        return Err(ExploreError::Invalid(
            "manifest has no [explore] section".to_owned(),
        ));
    }
    if !plan.events.is_empty() {
        return Err(ExploreError::Invalid(
            "serial [[events]] are not available during exploration; use explore.events with an SDK control-channel guest"
                .to_owned(),
        ));
    }
    if !plan.storage.is_empty() || plan.network.loopback || plan.network.drop_ppm != 0 || plan.network.partitioned {
        return Err(ExploreError::Invalid(
            "exploration currently accepts only the headless control-channel VM; storage and network settings are unavailable"
                .to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_manifest_without_an_exploration_contract() {
        let plan: RunPlan = serde_json::from_str(
            r#"{
                "format":"theseus-run-plan-v1", "manifest":"/tmp/theseus.toml",
                "runtime":{"firecracker":{"path":"/tmp/firecracker","sha256":"x"}},
                "guest":{"kernel":{"path":"/tmp/kernel","sha256":"x"},"initramfs":{"path":"/tmp/initramfs","sha256":"x"}},
                "run":{"seed":1,"vcpu_count":1,"mem_size_mib":128,"timeout_secs":1,"virtual_time":null},
                "events":[], "network":{"loopback":false,"drop_ppm":0,"partitioned":false}, "storage":[], "checks":[]
            }"#,
        )
        .unwrap();
        assert!(validate(&plan).unwrap_err().to_string().contains("no [explore]"));
    }
}
