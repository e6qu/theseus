// Copyright 2026 Adrian Mârza and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Offline integrity checks, not a claim that a bundle executed on native KVM.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize)]
pub struct TopologyBundleSummary {
    pub format: &'static str,
    pub architecture: String,
    pub starting_checkpoint_sha256: String,
    pub services: usize,
    pub runs: usize,
    pub native_execution_verified: bool,
}

struct Archive<'a> {
    scope: &'a Path,
    files: &'a BTreeMap<String, (String, u64)>,
    retained: &'a BTreeMap<String, Vec<u8>>,
}

struct Bundle<'a> {
    directory: PathBuf,
    archive: Option<Archive<'a>>,
}

impl Bundle<'_> {
    fn path(&self, base: &Path, relative: &str) -> Result<PathBuf, String> {
        if relative.contains('\\') || relative.contains('\0') {
            return Err("unsafe bundle path".into());
        }
        let mut path = base.to_path_buf();
        for component in Path::new(relative).components() {
            match component {
                Component::Normal(name) => path.push(name),
                Component::CurDir => (),
                Component::ParentDir if path.pop() => (),
                _ => return Err("artifact escapes the retained bundle".into()),
            }
        }
        if let Some(archive) = &self.archive {
            if !path.starts_with(archive.scope) {
                return Err("artifact escapes its archived replay bundle".into());
            }
            if !archive.files.contains_key(text_path(&path)?) {
                return Err(format!(
                    "missing archived bundle artifact: {}",
                    path.display()
                ));
            }
            return Ok(path);
        }
        let mut checked = self.directory.clone();
        for component in path.components() {
            checked.push(component);
            let metadata = fs::symlink_metadata(&checked).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("bundle artifacts must not traverse symlinks".into());
            }
        }
        if !checked.is_file() {
            return Err(format!(
                "missing regular bundle artifact: {}",
                path.display()
            ));
        }
        Ok(path)
    }

    fn json(&self, path: &Path) -> Result<Value, String> {
        let checked = self.path(Path::new(""), text_path(path)?)?;
        if let Some(archive) = &self.archive {
            return serde_json::from_slice(
                archive
                    .retained
                    .get(text_path(&checked)?)
                    .ok_or("missing archived bundle JSON")?,
            )
            .map_err(|error| error.to_string());
        }
        let mut bytes = Vec::new();
        fs::File::open(self.directory.join(checked))
            .and_then(|file| file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|error| error.to_string())?;
        if bytes.len() > 128 * 1024 * 1024 {
            return Err("bundle JSON exceeds 128 MiB".into());
        }
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())
    }

    fn digest(&self, path: &Path) -> Result<(String, u64), String> {
        let checked = self.path(Path::new(""), text_path(path)?)?;
        if let Some(archive) = &self.archive {
            return archive
                .files
                .get(text_path(&checked)?)
                .cloned()
                .ok_or("missing archive inventory entry".into());
        }
        let mut file =
            fs::File::open(self.directory.join(checked)).map_err(|error| error.to_string())?;
        let mut hash = Sha256::new();
        let mut bytes = 0;
        let mut buffer = [0; 65536];
        loop {
            let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            if bytes > 64 * 1024 * 1024 * 1024 {
                return Err("artifact exceeds 64 GiB".into());
            }
            hash.update(&buffer[..count]);
        }
        Ok((format!("{:x}", hash.finalize()), bytes))
    }

    fn artifact(&self, base: &Path, value: &Value) -> Result<PathBuf, String> {
        let path = self.path(base, string(&value["path"])?)?;
        let (actual, _) = self.digest(&path)?;
        if actual != string(&value["sha256"])? {
            return Err(format!("artifact hash differs: {}", path.display()));
        }
        Ok(path)
    }

    fn artifacts(&self, base: &Path, value: &Value) -> Result<(), String> {
        match value {
            Value::Object(map) => {
                if map.contains_key("path") && map.contains_key("sha256") {
                    self.artifact(base, value)?;
                }
                for child in map.values() {
                    self.artifacts(base, child)?;
                }
            }
            Value::Array(values) => {
                for child in values {
                    self.artifacts(base, child)?;
                }
            }
            _ => (),
        }
        Ok(())
    }

    fn member(
        &self,
        base: &Path,
        name: &str,
        expected: &Value,
        maximum: u64,
    ) -> Result<(), String> {
        let path = self.path(base, name)?;
        let (sha, bytes) = self.digest(&path)?;
        if bytes == 0
            || bytes > maximum
            || Some(bytes) != expected["bytes"].as_u64()
            || sha != string(&expected["sha256"])?
        {
            return Err(format!("checkpoint member differs: {}", path.display()));
        }
        Ok(())
    }

    fn root(&self, base: &Path, plan: &Value) -> Result<(String, Value), String> {
        if !matches!(
            plan["format"].as_str(),
            Some("theseus-compose-plan-v1" | "theseus-compose-replay-plan-v1")
        ) {
            return Err("unsupported topology replay plan format".into());
        }
        if plan["replay_start"] != "ready_checkpoint" {
            return Err("compose verify requires a retained ready-checkpoint bundle".into());
        }
        self.artifacts(base, plan)?;
        if plan["topology_runner"]["sha256"].as_str().is_none() {
            return Err("missing locked topology runtime".into());
        }
        let path = self.artifact(base, &plan["starting_checkpoint"])?;
        if path.file_name().and_then(|name| name.to_str()) != Some("metadata.json") {
            return Err("checkpoint manifest must be metadata.json".into());
        }
        let metadata = self.json(&path)?;
        if metadata["format"] != "theseus-topology-checkpoint-v1"
            || !matches!(string(&metadata["architecture"])?, "amd64" | "arm64")
        {
            return Err("unsupported topology checkpoint format or architecture".into());
        }
        let services = object(&plan["services"])?;
        if services.is_empty()
            || services.keys().ne(object(&metadata["services"])?.keys())
            || services
                .keys()
                .ne(object(&metadata["execution_prefixes"])?.keys())
            || services
                .keys()
                .ne(object(&plan["checkpoint_prefixes"])?.keys())
        {
            return Err("checkpoint service or ancestry set differs".into());
        }
        let parent = path.parent().ok_or("checkpoint has no parent")?;
        self.member(
            parent,
            "context.bin",
            &metadata["context"],
            128 * 1024 * 1024,
        )?;
        for (name, service) in services {
            safe_name(name)?;
            if !["tick_ns", "exits_per_tick"].iter().all(|field| {
                service["run"]["run"]["virtual_time"][field]
                    .as_u64()
                    .is_some_and(|value| value > 0)
            }) {
                return Err("ready checkpoint requires a locked virtual-time profile".into());
            }
            let member_base = parent.join(name);
            self.member(
                &member_base,
                "vmstate",
                &metadata["services"][name]["vmstate"],
                128 * 1024 * 1024,
            )?;
            let memory = service["run"]["run"]["mem_size_mib"]
                .as_u64()
                .ok_or("missing guest memory size")?;
            if memory == 0
                || memory > 65536
                || metadata["services"][name]["memory"]["bytes"].as_u64()
                    != Some(memory * 1024 * 1024)
            {
                return Err("checkpoint RAM does not match guest configuration".into());
            }
            self.member(
                &member_base,
                "memory",
                &metadata["services"][name]["memory"],
                64 * 1024 * 1024 * 1024,
            )?;
            let prefix = metadata["execution_prefixes"][name]
                .as_array()
                .ok_or("missing execution ancestry")?;
            if prefix.is_empty()
                || Some(prefix.len() as u64) != plan["checkpoint_prefixes"][name].as_u64()
            {
                return Err("checkpoint inherited decision count differs".into());
            }
        }
        if configuration(plan)? != string(&metadata["configuration_sha256"])? {
            return Err("checkpoint configuration identity differs".into());
        }
        Ok((
            string(&plan["starting_checkpoint"]["sha256"])?.into(),
            metadata,
        ))
    }

    fn results(
        &self,
        base: &Path,
        plan: &Value,
        sha: &str,
        metadata: &Value,
        aggregate: Option<&Value>,
    ) -> Result<(), String> {
        for (name, profile) in object(&plan["services"])? {
            let directory = base.join("services").join(name);
            let result = self.json(&directory.join("result.json"))?;
            let prefix = metadata["execution_prefixes"][name]
                .as_array()
                .ok_or("missing ancestry")?;
            let trace = result["machine_execution_trace"]
                .as_array()
                .ok_or("missing complete machine stream")?;
            if !result["error"].is_null()
                || !matches!(result["status"].as_str(), Some("passed" | "failed"))
                || result["execution_start"]["kind"] != "topology_checkpoint"
                || result["execution_start"]["checkpoint_sha256"] != sha
                || result["execution_start"]["inherited_decisions"].as_u64()
                    != Some(prefix.len() as u64)
                || trace.len() <= prefix.len()
                || !trace.starts_with(prefix)
            {
                return Err(format!(
                    "invalid execution ancestry or resumed suffix for {name}"
                ));
            }
            let evidence: crate::execution::Evidence = serde_json::from_value(json!({
                "format": "theseus-execution-v1", "boundary": "pause", "replay_error": null,
                "execution_ledgers": result["execution_ledgers"],
                "machine_execution_ledger": result["machine_execution_ledger"],
                "machine_execution_trace": result["machine_execution_trace"],
            }))
            .map_err(|error| error.to_string())?;
            let cpus = profile["run"]["run"]["vcpu_count"]
                .as_u64()
                .filter(|n| (1..=32).contains(n))
                .ok_or("invalid vCPU count")?;
            evidence.validate(cpus as u8)?;
            let uart = result["serial_sha256"]
                .as_array()
                .ok_or("missing UART hashes")?;
            if uart.is_empty() || uart.len() > 2 {
                return Err("invalid UART hash count".into());
            }
            for (index, expected) in result["serial_sha256"]
                .as_array()
                .ok_or("missing UART hashes")?
                .iter()
                .enumerate()
            {
                let name = if index == 0 {
                    "serial.log".into()
                } else {
                    format!("serial-{index}.log")
                };
                if self.digest(&directory.join(&name))?.0 != string(expected)? {
                    return Err(format!(
                        "retained UART log hash differs: {}",
                        directory.join(name).display()
                    ));
                }
            }
            if let Some(run) = aggregate {
                for (field, aggregate_field) in [
                    ("execution_ledgers", "execution_ledgers"),
                    ("machine_execution_ledger", "machine_execution_ledgers"),
                    ("machine_execution_trace", "machine_execution_traces"),
                ] {
                    if result[field] != run[aggregate_field][name] {
                        return Err(
                            "campaign ledger does not cover the complete service stream".into()
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

fn string(value: &Value) -> Result<&str, String> {
    value
        .as_str()
        .ok_or_else(|| "missing string in retained evidence".into())
}
fn object(value: &Value) -> Result<&serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "missing object in retained evidence".into())
}
fn text_path(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| "non-UTF-8 bundle path".into())
}
fn safe_name(name: &str) -> Result<(), String> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\']) {
        Err("unsafe service name".into())
    } else {
        Ok(())
    }
}

fn configuration(plan: &Value) -> Result<String, String> {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if map.contains_key("sha256") {
                    map.remove("path");
                }
                for child in map.values_mut() {
                    strip(child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    strip(child);
                }
            }
            _ => (),
        }
    }
    let mut services = BTreeMap::new();
    for (name, profile) in object(&plan["services"])? {
        let mut run = profile["run"].clone();
        let map = run.as_object_mut().ok_or("missing run configuration")?;
        for key in ["manifest", "events", "checks"] {
            map.remove(key);
        }
        strip(&mut run);
        services.insert(name, json!({"run": run, "networks": profile["networks"]}));
    }
    let bytes = serde_json::to_vec(&json!({"services": services, "networks": plan["networks"],
        "runtime": plan["topology_runner"]["sha256"]}))
    .map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Verify locked inputs, checkpoint members, ancestry, and full execution hashes.
/// This does not deserialize KVM state or establish native execution provenance.
pub fn verify_topology_bundle(path: impl AsRef<Path>) -> Result<TopologyBundleSummary, String> {
    let path = path.as_ref();
    if !fs::symlink_metadata(path)
        .map_err(|error| error.to_string())?
        .file_type()
        .is_dir()
    {
        return Err("bundle root must be a directory, not a symlink".into());
    }
    let bundle = Bundle {
        directory: path.to_path_buf(),
        archive: None,
    };
    verify_contents(
        &bundle,
        Path::new(""),
        path.join("campaign-result.json").exists(),
    )
}

/// The caller has already hashed the archive and rejected duplicate/unsafe entries.
pub(crate) fn verify_archived_bundle(
    scope: &str,
    files: &BTreeMap<String, (String, u64)>,
    retained: &BTreeMap<String, Vec<u8>>,
) -> Result<TopologyBundleSummary, String> {
    let base = Path::new(scope);
    let bundle = Bundle {
        directory: PathBuf::new(),
        archive: Some(Archive {
            scope: base,
            files,
            retained,
        }),
    };
    verify_contents(
        &bundle,
        base,
        files.contains_key(text_path(&base.join("campaign-result.json"))?),
    )
}

fn verify_contents(
    bundle: &Bundle<'_>,
    base: &Path,
    campaign_exists: bool,
) -> Result<TopologyBundleSummary, String> {
    let plan = bundle.json(&base.join("replay-plan.json"))?;
    let (sha, metadata) = bundle.root(base, &plan)?;
    let runs = if campaign_exists {
        let campaign = bundle.json(&base.join("campaign-result.json"))?;
        if campaign["starting_checkpoint_sha256"] != sha {
            return Err("campaign root identity differs".into());
        }
        let runs = campaign["runs"].as_array().ok_or("missing campaign runs")?;
        if runs.is_empty() {
            return Err("empty campaign witness".into());
        }
        for (index, run) in runs.iter().enumerate() {
            if run["index"].as_u64() != Some(index as u64) {
                return Err("campaign run indices differ".into());
            }
            let directory = base.join(format!("runs/{index:03}"));
            let run_plan = bundle.json(&directory.join("replay-plan.json"))?;
            let (run_sha, run_metadata) = bundle.root(&directory, &run_plan)?;
            if run_sha != sha {
                return Err("campaign child has a different checkpoint root".into());
            }
            bundle.results(&directory, &run_plan, &run_sha, &run_metadata, Some(run))?;
        }
        runs.len()
    } else {
        bundle.results(base, &plan, &sha, &metadata, None)?;
        1
    };
    Ok(TopologyBundleSummary {
        format: "theseus-topology-bundle-verification-v1",
        architecture: string(&metadata["architecture"])?.into(),
        starting_checkpoint_sha256: sha,
        services: object(&plan["services"])?.len(),
        runs,
        native_execution_verified: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, bytes: &[u8]) -> Value {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        json!({"path": name, "sha256": format!("{:x}", Sha256::digest(bytes)), "bytes": bytes.len()})
    }

    fn ledger(records: &[&str]) -> Value {
        let mut hash = Sha256::new();
        for record in records {
            hash.update((record.len() as u64).to_le_bytes());
            hash.update(record.as_bytes());
        }
        json!({"decisions": records.len(), "sha256": format!("{:x}", hash.finalize()), "tail": records})
    }

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let input = write(root, "artifacts/runner", b"retained runtime");
        let prefix = ["vcpu:0:mmio_write:0x10:1:2a"];
        let context = write(root, "checkpoint/context.bin", b"opaque context");
        let state = write(root, "checkpoint/service/vmstate", b"opaque KVM state");
        let memory = write(root, "checkpoint/service/memory", &vec![0; 1024 * 1024]);
        let mut plan = json!({"format": "theseus-compose-replay-plan-v1", "replay_start": "ready_checkpoint", "networks": {}, "topology_runner": input,
            "services": {"service": {"networks": [], "run": {"format": "theseus-run-plan-v1",
                "manifest": "original.toml", "events": [], "checks": [],
                "run": {"mem_size_mib": 1, "vcpu_count": 1, "virtual_time": {"tick_ns": 1000, "exits_per_tick": 10}}}}}, "checkpoint_prefixes": {"service": 1}});
        let metadata = json!({"format": "theseus-topology-checkpoint-v1", "architecture": "amd64",
            "configuration_sha256": configuration(&plan).unwrap(), "context": context,
            "services": {"service": {"vmstate": state, "memory": memory}}, "execution_prefixes": {"service": prefix}});
        plan["starting_checkpoint"] = write(
            root,
            "checkpoint/metadata.json",
            &serde_json::to_vec(&metadata).unwrap(),
        );
        write(
            root,
            "replay-plan.json",
            &serde_json::to_vec(&plan).unwrap(),
        );
        let log = write(root, "services/service/serial.log", b"ready\n42\n");
        let trace = [prefix[0], "host:serial_input:2:2a0a"];
        let result = json!({"status": "passed", "error": null, "execution_start": {
            "kind": "topology_checkpoint", "checkpoint_sha256": plan["starting_checkpoint"]["sha256"], "inherited_decisions": 1},
            "machine_execution_trace": trace, "machine_execution_ledger": ledger(&trace),
            "execution_ledgers": [ledger(&["mmio_write:0x10:1:2a"])], "serial_sha256": [log["sha256"]]});
        write(
            root,
            "services/service/result.json",
            &serde_json::to_vec(&result).unwrap(),
        );
        directory
    }

    #[test]
    fn portable_root_verifies_without_claiming_native_execution() {
        let fixture = fixture();
        let summary = verify_topology_bundle(fixture.path()).unwrap();
        assert_eq!(summary.services, 1);
        assert!(!summary.native_execution_verified);
        let relocated = tempfile::tempdir().unwrap();
        fs::rename(fixture.path(), relocated.path().join("bundle")).unwrap();
        assert!(verify_topology_bundle(relocated.path().join("bundle")).is_ok());
    }

    #[test]
    fn tampered_ram_inputs_logs_and_complete_ledgers_fail() {
        for name in [
            "checkpoint/service/memory",
            "artifacts/runner",
            "services/service/serial.log",
        ] {
            let fixture = fixture();
            fs::write(fixture.path().join(name), b"changed").unwrap();
            assert!(verify_topology_bundle(fixture.path()).is_err(), "{name}");
        }
        for field in [
            "machine_execution_ledger",
            "execution_ledgers",
            "execution_start",
            "machine_execution_trace",
        ] {
            let fixture = fixture();
            let path = fixture.path().join("services/service/result.json");
            let mut result: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            result[field] = Value::Null;
            fs::write(path, serde_json::to_vec(&result).unwrap()).unwrap();
            assert!(verify_topology_bundle(fixture.path()).is_err(), "{field}");
        }
    }

    #[test]
    fn changed_configuration_or_escaping_root_is_rejected() {
        let fixture = fixture();
        let path = fixture.path().join("replay-plan.json");
        let mut plan: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        plan["services"]["service"]["run"]["run"]["seed"] = json!(99);
        fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
        assert!(verify_topology_bundle(fixture.path())
            .unwrap_err()
            .contains("configuration"));
        plan["starting_checkpoint"]["path"] = json!("../checkpoint/metadata.json");
        fs::write(path, serde_json::to_vec(&plan).unwrap()).unwrap();
        assert!(verify_topology_bundle(fixture.path())
            .unwrap_err()
            .contains("escapes"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_members_are_not_portable_artifacts() {
        let fixture = fixture();
        let path = fixture.path().join("checkpoint/service/memory");
        fs::rename(&path, path.with_extension("original")).unwrap();
        std::os::unix::fs::symlink("memory.original", path).unwrap();
        assert!(verify_topology_bundle(fixture.path())
            .unwrap_err()
            .contains("symlinks"));
    }

    #[test]
    fn child_paths_may_share_a_root_but_not_escape_the_campaign() {
        let fixture = fixture();
        let bundle = Bundle {
            directory: fixture.path().into(),
            archive: None,
        };
        assert_eq!(
            bundle
                .path(Path::new("runs/000"), "../../checkpoint/metadata.json")
                .unwrap(),
            Path::new("checkpoint/metadata.json")
        );
        assert!(bundle
            .path(Path::new("runs/000"), "../../../checkpoint/metadata.json")
            .is_err());
    }

    #[test]
    fn campaign_aggregates_must_bind_complete_child_evidence() {
        let fixture = fixture();
        let root = fixture.path();
        let mut plan: Value =
            serde_json::from_slice(&fs::read(root.join("replay-plan.json")).unwrap()).unwrap();
        let result: Value =
            serde_json::from_slice(&fs::read(root.join("services/service/result.json")).unwrap())
                .unwrap();
        plan["topology_runner"]["path"] = json!("../../artifacts/runner");
        plan["starting_checkpoint"]["path"] = json!("../../checkpoint/metadata.json");
        write(
            root,
            "runs/000/replay-plan.json",
            &serde_json::to_vec(&plan).unwrap(),
        );
        write(
            root,
            "runs/000/services/service/result.json",
            &serde_json::to_vec(&result).unwrap(),
        );
        write(root, "runs/000/services/service/serial.log", b"ready\n42\n");
        let mut campaign = json!({"starting_checkpoint_sha256": plan["starting_checkpoint"]["sha256"], "runs": [{
            "index": 0, "execution_ledgers": {"service": result["execution_ledgers"]},
            "machine_execution_ledgers": {"service": result["machine_execution_ledger"]},
            "machine_execution_traces": {"service": result["machine_execution_trace"]}}]});
        write(
            root,
            "campaign-result.json",
            &serde_json::to_vec(&campaign).unwrap(),
        );
        assert_eq!(verify_topology_bundle(root).unwrap().runs, 1);
        campaign["runs"][0]["machine_execution_ledgers"]["service"] =
            ledger(&["vcpu:0:mmio_write:0x10:1:2a"]);
        write(
            root,
            "campaign-result.json",
            &serde_json::to_vec(&campaign).unwrap(),
        );
        assert!(verify_topology_bundle(root)
            .unwrap_err()
            .contains("complete service stream"));
        campaign["runs"][0]["machine_execution_ledgers"]["service"] =
            result["machine_execution_ledger"].clone();
        campaign["runs"][0]["index"] = json!(1);
        write(
            root,
            "campaign-result.json",
            &serde_json::to_vec(&campaign).unwrap(),
        );
        assert!(verify_topology_bundle(root)
            .unwrap_err()
            .contains("indices"));
    }

    #[test]
    fn archived_roots_are_checked_without_extracting_ram() {
        fn inventory(
            root: &Path,
            directory: &Path,
            files: &mut BTreeMap<String, (String, u64)>,
            retained: &mut BTreeMap<String, Vec<u8>>,
        ) {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    inventory(root, &path, files, retained);
                    continue;
                }
                let bytes = fs::read(&path).unwrap();
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                files.insert(
                    name.clone(),
                    (format!("{:x}", Sha256::digest(&bytes)), bytes.len() as u64),
                );
                if name.ends_with(".json") {
                    retained.insert(name, bytes);
                }
            }
        }
        let fixture = fixture();
        let mut files = BTreeMap::new();
        let mut retained = BTreeMap::new();
        inventory(fixture.path(), fixture.path(), &mut files, &mut retained);
        assert!(verify_archived_bundle("", &files, &retained).is_ok());
        files.get_mut("checkpoint/service/memory").unwrap().0 = "0".repeat(64);
        assert!(verify_archived_bundle("", &files, &retained)
            .unwrap_err()
            .contains("member differs"));
        files.remove("checkpoint/service/memory");
        assert!(verify_archived_bundle("", &files, &retained)
            .unwrap_err()
            .contains("missing archived"));
    }
}
