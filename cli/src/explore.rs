// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Product boundary for the Linux/KVM exploration executor.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::{load_plan, ArtifactPlan, LoadError, ReplayFingerprint, ReplayTreeNode, RunPlan};

#[derive(Debug)]
pub enum ExploreError {
    Manifest(LoadError),
    Invalid(String),
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
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
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "cannot write {}: {source}", path.display())
            }
            Self::Execute(reason) => write!(formatter, "exploration failed: {reason}"),
        }
    }
}

impl std::error::Error for ExploreError {}

#[derive(Clone, Copy)]
enum ExplorerAction {
    Explore,
    Minimize,
    Snapshot,
}

/// Start a single-manifest exploration through the executor released beside the CLI.
pub fn explore(path: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<PathBuf, ExploreError> {
    let plan = load_plan(path).map_err(ExploreError::Manifest)?;
    validate(&plan)?;
    execute_plan(&plan, output, ExplorerAction::Explore)
}

/// Re-run a recorded exploration using only its locked artifacts.
pub fn replay_exploration(
    bundle: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ExploreError> {
    let plan = verified_whole_tree_replay_plan(bundle)?;
    execute_plan(&plan, output, ExplorerAction::Explore)
}

/// Re-run one recorded root-to-node path using only locked artifacts. The
/// path includes the root seed and is the compact reproducer printed in an
/// exploration result and report.
pub fn replay_exploration_path(
    bundle: impl AsRef<Path>,
    seed_path: Vec<u64>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ExploreError> {
    let plan = verified_targeted_replay_plan(bundle, seed_path)?;
    execute_plan(&plan, output, ExplorerAction::Explore)
}

/// Reduce `explore.events` for a property-failing recorded path. The output
/// remains a locked bundle and preserves exactly the same failed check names.
pub fn minimize_exploration_path(
    bundle: impl AsRef<Path>,
    seed_path: Vec<u64>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ExploreError> {
    let plan = targeted_replay_plan(bundle, seed_path)?;
    execute_plan(&plan, output, ExplorerAction::Minimize)
}

/// Export one recorded root-to-node timeline as Firecracker-compatible state
/// and memory files, using only its locked artifacts.
pub fn snapshot_exploration_path(
    bundle: impl AsRef<Path>,
    seed_path: Vec<u64>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ExploreError> {
    let plan = verified_targeted_replay_plan(bundle, seed_path)?;
    execute_plan(&plan, output, ExplorerAction::Snapshot)
}

fn verified_targeted_replay_plan(
    bundle: impl AsRef<Path>,
    seed_path: Vec<u64>,
) -> Result<RunPlan, ExploreError> {
    if seed_path.is_empty() {
        return Err(ExploreError::Invalid(
            "seed path must include the root seed".to_owned(),
        ));
    }
    let expected = recorded_fingerprint(bundle.as_ref(), &seed_path)?;
    let mut plan = targeted_replay_plan(bundle, seed_path)?;
    plan.explore
        .as_mut()
        .expect("locked exploration plan was validated")
        .replay_expected = Some(expected);
    Ok(plan)
}

fn verified_whole_tree_replay_plan(bundle: impl AsRef<Path>) -> Result<RunPlan, ExploreError> {
    let expected_tree = recorded_tree(bundle.as_ref())?;
    let mut plan = locked_replay_plan(bundle)?;
    plan.explore
        .as_mut()
        .expect("locked exploration plan was validated")
        .replay_expected_tree = Some(expected_tree);
    Ok(plan)
}

#[derive(Deserialize)]
struct RecordedResult {
    format: String,
    #[serde(default)]
    nodes: Vec<RecordedNode>,
}

#[derive(Deserialize)]
struct RecordedNode {
    seed_path: Vec<u64>,
    entropy_probe_hex: String,
    markers_hex: String,
    dirty_pages: Option<u64>,
}

fn recorded_tree(bundle: &Path) -> Result<Vec<ReplayTreeNode>, ExploreError> {
    let result = recorded_result(bundle)?;
    Ok(result
        .nodes
        .into_iter()
        .map(|node| ReplayTreeNode {
            seed_path: node.seed_path,
            fingerprint: ReplayFingerprint {
                entropy_probe_hex: node.entropy_probe_hex,
                markers_hex: node.markers_hex,
                dirty_pages: node.dirty_pages,
            },
        })
        .collect())
}

fn recorded_fingerprint(
    bundle: &Path,
    seed_path: &[u64],
) -> Result<ReplayFingerprint, ExploreError> {
    let result = recorded_result(bundle)?;
    let node = result
        .nodes
        .into_iter()
        .find(|node| node.seed_path == seed_path)
        .ok_or_else(|| {
            ExploreError::Invalid(format!(
                "seed path is not recorded in {}",
                bundle.join("result.json").display()
            ))
        })?;
    Ok(ReplayFingerprint {
        entropy_probe_hex: node.entropy_probe_hex,
        markers_hex: node.markers_hex,
        dirty_pages: node.dirty_pages,
    })
}

fn recorded_result(bundle: &Path) -> Result<RecordedResult, ExploreError> {
    let bundle = fs::canonicalize(bundle).map_err(|source| ExploreError::Read {
        path: bundle.to_path_buf(),
        source,
    })?;
    let result_path = bundle.join("result.json");
    let result: RecordedResult = serde_json::from_slice(&fs::read(&result_path).map_err(
        |source| ExploreError::Read {
            path: result_path.clone(),
            source,
        },
    )?)
    .map_err(|error| {
        ExploreError::Invalid(format!("cannot parse {}: {error}", result_path.display()))
    })?;
    if result.format != "theseus-exploration-result-v1" {
        return Err(ExploreError::Invalid(format!(
            "result is not an exploration result: {}",
            result_path.display()
        )));
    }
    Ok(result)
}

fn targeted_replay_plan(
    bundle: impl AsRef<Path>,
    seed_path: Vec<u64>,
) -> Result<RunPlan, ExploreError> {
    if seed_path.is_empty() {
        return Err(ExploreError::Invalid(
            "seed path must include the root seed".to_owned(),
        ));
    }
    let mut plan = locked_replay_plan(bundle)?;
    if seed_path[0] != plan.run.seed {
        return Err(ExploreError::Invalid(format!(
            "seed path starts at {}, but this exploration starts at {}",
            seed_path[0], plan.run.seed
        )));
    }
    plan.explore
        .as_mut()
        .expect("locked exploration plan was validated")
        .replay_seed_path = Some(seed_path);
    Ok(plan)
}

fn locked_replay_plan(bundle: impl AsRef<Path>) -> Result<RunPlan, ExploreError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| ExploreError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    let plan_path = bundle.join("explore-plan.json");
    let mut plan: RunPlan =
        serde_json::from_slice(&fs::read(&plan_path).map_err(|source| ExploreError::Read {
            path: plan_path.clone(),
            source,
        })?)
        .map_err(|error| {
            ExploreError::Invalid(format!("cannot parse {}: {error}", plan_path.display()))
        })?;
    plan.runtime.firecracker = locked_artifact(&bundle, "firecracker", &plan.runtime.firecracker)?;
    plan.guest.kernel = locked_artifact(&bundle, "kernel", &plan.guest.kernel)?;
    plan.guest.initramfs = locked_artifact(&bundle, "initramfs", &plan.guest.initramfs)?;
    validate(&plan)?;
    Ok(plan)
}

fn execute_plan(
    plan: &RunPlan,
    output: impl AsRef<Path>,
    action: ExplorerAction,
) -> Result<PathBuf, ExploreError> {
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
        .ok_or_else(|| {
            ExploreError::Invalid("theseus binary has no parent directory".to_owned())
        })?;
    if !runner.is_file() {
        return Err(ExploreError::Invalid(format!(
            "missing Linux exploration runner beside theseus: {}; use a published Linux runtime bundle",
            runner.display()
        )));
    }
    let plan_file = output.with_extension("explore-plan.json");
    fs::write(
        &plan_file,
        serde_json::to_vec_pretty(&plan).map_err(|error| {
            ExploreError::Invalid(format!("cannot encode exploration plan: {error}"))
        })?,
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
        .args(match action {
            ExplorerAction::Explore => None,
            ExplorerAction::Minimize => Some("--minimize"),
            ExplorerAction::Snapshot => Some("--snapshot"),
        })
        .status()
        .map_err(|error| {
            ExploreError::Execute(format!("cannot start {}: {error}", runner.display()))
        })?;
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

fn locked_artifact(
    bundle: &Path,
    name: &str,
    expected: &ArtifactPlan,
) -> Result<ArtifactPlan, ExploreError> {
    let path = fs::canonicalize(bundle.join("artifacts").join(name)).map_err(|source| {
        ExploreError::Read {
            path: bundle.join("artifacts").join(name),
            source,
        }
    })?;
    if !path.starts_with(bundle) {
        return Err(ExploreError::Invalid(format!(
            "replay artifact escapes exploration bundle: {}",
            path.display()
        )));
    }
    Ok(ArtifactPlan {
        path: path.display().to_string(),
        sha256: expected.sha256.clone(),
    })
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
    if !plan.storage.is_empty()
        || plan.network.loopback
        || plan.network.drop_ppm != 0
        || plan.network.partitioned
    {
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
    use std::fs;

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
        assert!(validate(&plan)
            .unwrap_err()
            .to_string()
            .contains("no [explore]"));
    }

    #[test]
    fn replay_uses_the_bundle_copy_not_the_recorded_source_path() {
        let directory = tempfile::tempdir().unwrap();
        let artifacts = directory.path().join("artifacts");
        fs::create_dir(&artifacts).unwrap();
        fs::write(artifacts.join("kernel"), b"locked kernel").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let locked = locked_artifact(
            &root,
            "kernel",
            &ArtifactPlan {
                path: "/removed/source/kernel".to_owned(),
                sha256: "digest".to_owned(),
            },
        )
        .unwrap();
        assert!(Path::new(&locked.path).starts_with(root));
        assert_eq!(locked.sha256, "digest");
    }

    #[test]
    fn rejects_an_empty_targeted_replay_path() {
        let error = replay_exploration_path("missing-bundle", Vec::new(), "unused").unwrap_err();
        assert!(error.to_string().contains("root seed"));
    }

    #[test]
    fn reads_every_recorded_node_for_whole_tree_verification() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("result.json"),
            r#"{"format":"theseus-exploration-result-v1","nodes":[{"seed_path":[1],"entropy_probe_hex":"aa","markers_hex":"ff","dirty_pages":2},{"seed_path":[1,2],"entropy_probe_hex":"bb","markers_hex":"90ff","dirty_pages":3}]}"#,
        )
        .unwrap();

        let tree = recorded_tree(directory.path()).unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[1].seed_path, vec![1, 2]);
        assert_eq!(tree[1].fingerprint.entropy_probe_hex, "bb");
    }
}
