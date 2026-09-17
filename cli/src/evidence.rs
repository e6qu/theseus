// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Offline verification for native KVM evidence attached to a SHA release.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path};

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::Archive;

const INDEX_FORMAT: &str = "theseus-native-evidence-index-v2";
const PROOF_FORMAT: &str = "theseus-counterexample-proof-v2";
const VALIDATION_FORMAT_V1: &str = "theseus-runtime-validation-v1";
const VALIDATION_FORMAT_V2: &str = "theseus-runtime-validation-v2";
const VALIDATION_FORMAT_V3: &str = "theseus-runtime-validation-v3";
const VALIDATION_FORMAT_V4: &str = "theseus-runtime-validation-v4";
const VALIDATION_FORMAT_V5: &str = "theseus-runtime-validation-v5";
const CERTIFICATE_FORMAT_V1: &str = "theseus-runtime-certificate-v1";
const CERTIFICATE_FORMAT_V2: &str = "theseus-runtime-certificate-v2";
const CERTIFICATE_FORMAT_V3: &str = "theseus-runtime-certificate-v3";
const CERTIFICATE_FORMAT_V4: &str = "theseus-runtime-certificate-v4";
const CERTIFICATE_FORMAT_V5: &str = "theseus-runtime-certificate-v5";
const PROPERTY: &str = "distributed_lost_update_is_unreachable";
const REQUIRED_FAULTS: [&str; 2] = [
    "backplane:partition@setup",
    "backplane:heal@probe_partition",
];

#[derive(Debug)]
pub struct EvidenceError(String);

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EvidenceError {}

#[derive(Debug, PartialEq, Eq)]
pub struct NativeEvidenceSummary {
    pub source_commit: String,
    pub runtime_tag: String,
    pub architectures: Vec<String>,
}

#[derive(Deserialize)]
struct NativeEvidenceIndex {
    format: String,
    source_commit: String,
    runtime_tag: String,
    architectures: BTreeMap<String, ArchitectureEvidence>,
}

#[derive(Deserialize)]
struct ArchitectureEvidence {
    certificate: EvidenceAsset,
    counterexample: EvidenceAsset,
    validation: EvidenceAsset,
}

#[derive(Deserialize)]
struct EvidenceAsset {
    file: String,
    sha256: String,
    bytes: u64,
}

#[derive(Deserialize)]
struct RuntimeCertificate {
    format: String,
    status: String,
    profile: CertificateProfile,
    source: CertificateSource,
    repeatability: CertificateRepeatability,
    #[serde(default)]
    services: BTreeMap<String, CertificateServiceEvidence>,
}

#[derive(Deserialize)]
struct CertificateServiceEvidence {
    #[serde(default)]
    execution_start: Option<serde_json::Value>,
    #[serde(default)]
    execution_ledgers: Vec<ExecutionLedgerEvidence>,
    #[serde(default)]
    machine_execution_ledger: Option<ExecutionLedgerEvidence>,
    #[serde(default)]
    machine_execution_trace_decisions: Option<usize>,
}

#[derive(Deserialize)]
struct ExecutionLedgerEvidence {
    decisions: u64,
    sha256: String,
    #[serde(default)]
    tail: Vec<String>,
}

#[derive(Deserialize)]
struct CertificateSource {
    plan_sha256: String,
    plan_contents: String,
}

#[derive(Deserialize)]
struct CertificateProfile {
    id: String,
    architecture: String,
}

#[derive(Deserialize)]
struct CertificateRepeatability {
    executions: u8,
}

#[derive(Deserialize)]
struct CounterexampleProof {
    format: String,
    architecture: String,
    source_commit: String,
    runtime: ProofRuntime,
    host: ProofHost,
    property: String,
    required_faults: Vec<String>,
    files: BTreeMap<String, ProofFile>,
}

#[derive(Deserialize)]
struct ProofRuntime {
    image: String,
    tag: String,
}

#[derive(Deserialize)]
struct ProofHost {
    kernel_release: String,
    kvm_api_version: i64,
}

#[derive(Deserialize)]
struct ProofFile {
    sha256: String,
    bytes: u64,
}

#[derive(Deserialize)]
struct RuntimeValidationProof {
    format: String,
    architecture: String,
    source_commit: String,
    runtime: ProofRuntime,
    host: ProofHost,
    scenarios: Vec<String>,
    files: BTreeMap<String, ProofFile>,
}

/// Verify the signed index's files, certificates, archive inventories, and
/// required runtime observations without KVM.
pub fn verify_native_evidence(
    index_path: impl AsRef<Path>,
) -> Result<NativeEvidenceSummary, EvidenceError> {
    let index_path = index_path.as_ref();
    let bytes = read(index_path)?;
    let index: NativeEvidenceIndex = parse_json(index_path, &bytes)?;
    require(
        index.format == INDEX_FORMAT,
        "unsupported native evidence index format",
    )?;
    require_commit(&index.source_commit)?;
    require(
        index.runtime_tag == index.source_commit[..12],
        "native evidence tag does not match its source commit",
    )?;
    let supported = BTreeSet::from(["amd64".to_owned(), "arm64".to_owned()]);
    let expected = index.architectures.keys().cloned().collect::<BTreeSet<_>>();
    require(
        !expected.is_empty() && expected.is_subset(&supported),
        "native evidence index must contain amd64, arm64, or both",
    )?;
    let directory = index_path.parent().unwrap_or_else(|| Path::new("."));
    for (architecture, evidence) in &index.architectures {
        let certificate_name = format!(
            "theseus-{}-runtime-certificate-{architecture}.json",
            index.runtime_tag
        );
        let counterexample_name = format!(
            "theseus-{}-multiservice-counterexample-{architecture}.tar.gz",
            index.runtime_tag
        );
        let validation_name = format!(
            "theseus-{}-runtime-validation-{architecture}.tar.gz",
            index.runtime_tag
        );
        require(
            evidence.certificate.file == certificate_name,
            &format!("unexpected {architecture} certificate filename"),
        )?;
        require(
            evidence.counterexample.file == counterexample_name,
            &format!("unexpected {architecture} counterexample filename"),
        )?;
        require(
            evidence.validation.file == validation_name,
            &format!("unexpected {architecture} validation filename"),
        )?;
        let certificate_path = directory.join(&evidence.certificate.file);
        let certificate_bytes = verify_asset(&certificate_path, &evidence.certificate)?;
        verify_certificate(&certificate_path, &certificate_bytes, architecture)?;
        let archive_path = directory.join(&evidence.counterexample.file);
        verify_asset_streaming(&archive_path, &evidence.counterexample)?;
        verify_counterexample(
            &archive_path,
            architecture,
            &index.source_commit,
            &index.runtime_tag,
            &certificate_bytes,
        )?;
        let validation_path = directory.join(&evidence.validation.file);
        verify_asset_streaming(&validation_path, &evidence.validation)?;
        verify_runtime_validation(
            &validation_path,
            architecture,
            &index.source_commit,
            &index.runtime_tag,
            &certificate_bytes,
        )?;
    }
    Ok(NativeEvidenceSummary {
        source_commit: index.source_commit,
        runtime_tag: index.runtime_tag,
        architectures: expected.into_iter().collect(),
    })
}

fn verify_asset(path: &Path, asset: &EvidenceAsset) -> Result<Vec<u8>, EvidenceError> {
    let bytes = read(path)?;
    require(
        bytes.len() as u64 == asset.bytes,
        &format!("asset size differs from index: {}", path.display()),
    )?;
    require_sha256(&asset.sha256, "asset")?;
    require(
        sha256(&bytes) == asset.sha256,
        &format!("asset digest differs from index: {}", path.display()),
    )?;
    Ok(bytes)
}

fn verify_asset_streaming(path: &Path, asset: &EvidenceAsset) -> Result<(), EvidenceError> {
    let metadata = fs::metadata(path)
        .map_err(|error| EvidenceError(format!("cannot inspect {}: {error}", path.display())))?;
    require(
        metadata.len() == asset.bytes,
        &format!("asset size differs from index: {}", path.display()),
    )?;
    require_sha256(&asset.sha256, "asset")?;
    let mut file = fs::File::open(path)
        .map_err(|error| EvidenceError(format!("cannot read {}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|error| EvidenceError(format!("cannot hash {}: {error}", path.display())))?;
    require(
        format!("{:x}", hasher.finalize()) == asset.sha256,
        &format!("asset digest differs from index: {}", path.display()),
    )
}

fn verify_certificate(path: &Path, bytes: &[u8], architecture: &str) -> Result<(), EvidenceError> {
    let certificate: RuntimeCertificate = parse_json(path, bytes)?;
    require(
        matches!(
            certificate.format.as_str(),
            CERTIFICATE_FORMAT_V1
                | CERTIFICATE_FORMAT_V2
                | CERTIFICATE_FORMAT_V3
                | CERTIFICATE_FORMAT_V4
                | CERTIFICATE_FORMAT_V5
        ),
        "unsupported runtime certificate format",
    )?;
    require(
        certificate.status == "passed",
        "runtime certificate did not pass",
    )?;
    require(
        certificate.profile.id == "linux-kvm-simulated-io-v1",
        "runtime certificate has the wrong support profile",
    )?;
    require(
        certificate.profile.architecture == architecture,
        "runtime certificate architecture differs from its asset",
    )?;
    require_sha256(&certificate.source.plan_sha256, "certificate plan")?;
    require(
        sha256(certificate.source.plan_contents.as_bytes()) == certificate.source.plan_sha256,
        "runtime certificate plan digest does not match its embedded plan",
    )?;
    let plan: serde_json::Value = parse_json_bytes(
        certificate.source.plan_contents.as_bytes(),
        "certificate plan",
    )?;
    require(
        plan["format"] == "theseus-compose-plan-v1"
            && plan["services"]
                .as_object()
                .is_some_and(|services| !services.is_empty())
            && plan.get("campaign").is_none_or(serde_json::Value::is_null),
        "runtime certificate does not embed a fixed topology plan",
    )?;
    require(
        certificate.repeatability.executions == 2,
        "runtime certificate must record two executions",
    )?;
    if certificate.format == CERTIFICATE_FORMAT_V2 {
        require(
            !certificate.services.is_empty()
                && certificate.services.values().all(|service| {
                    !service.execution_ledgers.is_empty()
                        && service.execution_ledgers.iter().all(valid_execution_ledger)
                }),
            "runtime certificate has empty or malformed ordered KVM execution evidence",
        )?;
    }
    if matches!(
        certificate.format.as_str(),
        CERTIFICATE_FORMAT_V3 | CERTIFICATE_FORMAT_V4 | CERTIFICATE_FORMAT_V5
    ) {
        require(
            !certificate.services.is_empty()
                && certificate.services.values().all(|service| {
                    !service.execution_ledgers.is_empty()
                        && service.execution_ledgers.iter().all(valid_execution_ledger)
                        && service
                            .machine_execution_ledger
                            .as_ref()
                            .is_some_and(valid_execution_ledger)
                }),
            "runtime certificate has empty or malformed machine-wide execution evidence",
        )?;
    }
    if matches!(
        certificate.format.as_str(),
        CERTIFICATE_FORMAT_V4 | CERTIFICATE_FORMAT_V5
    ) {
        require(
            certificate.services.values().all(|service| {
                service
                    .machine_execution_trace_decisions
                    .is_some_and(|count| {
                        count > 0
                            && service
                                .machine_execution_ledger
                                .as_ref()
                                .is_some_and(|ledger| {
                                    usize::try_from(ledger.decisions) == Ok(count)
                                })
                    })
            }),
            "runtime certificate has missing or inconsistent active execution replay evidence",
        )?;
    }
    if certificate.format == CERTIFICATE_FORMAT_V5 {
        require(
            plan["replay_start"] == "ready_checkpoint",
            "checkpoint certificate lacks its explicit starting-state contract",
        )?;
        let checkpoint = &plan["starting_checkpoint"]["sha256"];
        let identity = checkpoint.as_str().ok_or_else(|| {
            EvidenceError("checkpoint certificate lacks its locked identity".to_owned())
        })?;
        require_sha256(identity, "topology checkpoint")?;
        require(
            certificate
                .services
                .keys()
                .eq(plan["services"].as_object().unwrap().keys()),
            "checkpoint certificate service set differs from its plan",
        )?;
        require(certificate.services.iter().all(|(name, service)| {
            service.execution_start.as_ref().is_some_and(|start| {
                start["kind"] == "topology_checkpoint" && start["checkpoint_sha256"] == identity
                    && start["inherited_decisions"].as_u64().is_some_and(|count| {
                        count > 0 && plan["checkpoint_prefixes"][name].as_u64() == Some(count)
                            && service.machine_execution_trace_decisions.is_some_and(|total| count < total as u64)
                    })
            })
        }), "checkpoint certificate lacks a bound inherited prefix and nonempty actively replayed suffix")?;
    } else {
        require(
            plan["starting_checkpoint"].is_null()
                && plan["replay_start"] != "ready_checkpoint"
                && certificate
                    .services
                    .values()
                    .all(|service| service.execution_start.is_none()),
            "fresh-boot certificate cannot claim checkpoint ancestry",
        )?;
    }
    Ok(())
}

fn valid_execution_ledger(ledger: &ExecutionLedgerEvidence) -> bool {
    ledger.decisions > 0
        && ledger.sha256.len() == 64
        && ledger
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && !ledger.tail.is_empty()
}

fn verify_counterexample(
    path: &Path,
    architecture: &str,
    source_commit: &str,
    runtime_tag: &str,
    certificate_bytes: &[u8],
) -> Result<(), EvidenceError> {
    let file = fs::File::open(path)
        .map_err(|error| EvidenceError(format!("cannot read {}: {error}", path.display())))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    let mut files = BTreeMap::new();
    let mut retained = BTreeMap::<String, Vec<u8>>::new();
    let required = BTreeSet::from([
        "evidence/proof.json",
        "evidence/runtime-certificate.json",
        "evidence/campaign-result.json",
        "evidence/replay/topology-result.json",
        "evidence/replay/services/counter/serial.log",
        "evidence/replay/services/writer-a/serial.log",
        "evidence/replay/services/counter/result.json",
        "evidence/replay/services/writer-a/result.json",
        "evidence/replay/services/writer-b/result.json",
        "minimization.json",
    ]);
    let entries = archive
        .entries()
        .map_err(|error| EvidenceError(format!("cannot open {}: {error}", path.display())))?;
    for entry in entries {
        let mut entry = entry
            .map_err(|error| EvidenceError(format!("cannot read {}: {error}", path.display())))?;
        let entry_path = entry
            .path()
            .map_err(|error| EvidenceError(format!("invalid archive path: {error}")))?;
        require_safe_archive_path(&entry_path)?;
        let relative = entry_path.strip_prefix("minimized").map_err(|_| {
            EvidenceError("counterexample archive root must be minimized".to_owned())
        })?;
        if relative.as_os_str().is_empty() || entry.header().entry_type().is_dir() {
            continue;
        }
        require(
            entry.header().entry_type().is_file(),
            "counterexample archive may contain only files and directories",
        )?;
        let name = relative
            .to_str()
            .ok_or_else(|| EvidenceError("counterexample path is not UTF-8".to_owned()))?
            .to_owned();
        let mut hasher = Sha256::new();
        let mut bytes = Vec::new();
        if required.contains(name.as_str()) || checkpoint_bundle_json(&name) {
            require(
                entry.header().size().unwrap_or(u64::MAX) <= 128 * 1024 * 1024,
                "retained counterexample JSON exceeds 128 MiB",
            )?;
            entry.read_to_end(&mut bytes).map_err(|error| {
                EvidenceError(format!("cannot read archive member {name}: {error}"))
            })?;
            hasher.update(&bytes);
            require(
                retained
                    .values()
                    .map(Vec::len)
                    .sum::<usize>()
                    .saturating_add(bytes.len())
                    <= 512 * 1024 * 1024,
                "retained counterexample JSON exceeds 512 MiB total",
            )?;
            retained.insert(name.clone(), bytes);
        } else {
            std::io::copy(&mut entry, &mut hasher).map_err(|error| {
                EvidenceError(format!("cannot hash archive member {name}: {error}"))
            })?;
        }
        let size = entry.header().size().unwrap_or(0);
        require(
            files
                .insert(
                    name,
                    ProofFile {
                        sha256: format!("{:x}", hasher.finalize()),
                        bytes: size,
                    },
                )
                .is_none(),
            "counterexample archive contains duplicate paths",
        )?;
    }
    require(
        required.iter().all(|name| retained.contains_key(*name)),
        "counterexample archive is missing required evidence",
    )?;
    let proof: CounterexampleProof = parse_json_bytes(&retained["evidence/proof.json"], "proof")?;
    require(
        proof.format == PROOF_FORMAT,
        "unsupported counterexample proof format",
    )?;
    require(
        proof.architecture == architecture,
        "counterexample architecture differs from its asset",
    )?;
    require(
        proof.source_commit == source_commit,
        "counterexample source commit differs from its index",
    )?;
    require(
        proof.runtime.tag == format!("{runtime_tag}-{architecture}"),
        "counterexample runtime tag differs from its index",
    )?;
    require_digest_reference(&proof.runtime.image)?;
    require(
        !proof.host.kernel_release.is_empty(),
        "counterexample has no host kernel release",
    )?;
    require(
        proof.host.kvm_api_version == 12,
        "counterexample was not produced by KVM API version 12",
    )?;
    require(
        proof.property == PROPERTY,
        "counterexample proof names the wrong property",
    )?;
    require(
        proof.required_faults == REQUIRED_FAULTS.map(str::to_owned),
        "counterexample proof does not retain the required recovery path",
    )?;
    let proof_file = files
        .remove("evidence/proof.json")
        .expect("retained proof was indexed");
    let _ = proof_file;
    require(
        proof.files.len() == files.len()
            && proof.files.iter().all(|(name, expected)| {
                files.get(name).is_some_and(|actual| {
                    expected.sha256 == actual.sha256 && expected.bytes == actual.bytes
                })
            }),
        "counterexample archive inventory does not match its contents",
    )?;
    require(
        retained["evidence/runtime-certificate.json"] == certificate_bytes,
        "counterexample embeds a different runtime certificate",
    )?;
    verify_campaign(&retained["evidence/campaign-result.json"])?;
    verify_minimization(&retained["minimization.json"])?;
    verify_archived_checkpoint_if_present("", &files, &retained, architecture)?;
    verify_archived_checkpoint_if_present("evidence/replay", &files, &retained, architecture)?;
    verify_replay(&retained)
}

fn verify_campaign(bytes: &[u8]) -> Result<(), EvidenceError> {
    let value: serde_json::Value = parse_json_bytes(bytes, "campaign result")?;
    require(
        value["format"] == "theseus-compose-campaign-result-v1",
        "unsupported campaign result format",
    )?;
    let properties = value["properties"]
        .as_array()
        .ok_or_else(|| EvidenceError("campaign result has no properties".to_owned()))?;
    for (name, status) in [
        ("network_recovery_is_reachable", "passed"),
        ("sequential_result_is_reachable", "passed"),
        (PROPERTY, "failed"),
    ] {
        require(
            properties
                .iter()
                .any(|property| property["name"] == name && property["status"] == status),
            &format!("campaign property {name} does not have status {status}"),
        )?;
    }
    Ok(())
}

fn verify_minimization(bytes: &[u8]) -> Result<(), EvidenceError> {
    let value: serde_json::Value = parse_json_bytes(bytes, "minimization")?;
    require(
        value["property"] == PROPERTY,
        "minimization names the wrong property",
    )?;
    let faults = value["minimized_faults"]
        .as_array()
        .ok_or_else(|| EvidenceError("minimization has no retained faults".to_owned()))?;
    require(
        REQUIRED_FAULTS
            .iter()
            .all(|required| faults.iter().any(|fault| fault.as_str() == Some(required))),
        "minimization removed a required recovery fault",
    )
}

fn verify_api_origin(
    plan: &serde_json::Value,
    execution: &crate::execution::Evidence,
    files: &BTreeMap<String, ProofFile>,
    retained: &BTreeMap<String, Vec<u8>>,
    architecture: &str,
) -> Result<(), EvidenceError> {
    if plan["format"] == "theseus-replay-plan-v2" {
        return require(
            execution.start.is_none()
                && plan["checkpoint"].is_null()
                && matches!(
                    plan["run"]["replay_start"].as_str(),
                    None | Some("fresh_boot")
                ),
            "fresh-boot replay cannot claim checkpoint ancestry",
        );
    }
    require(
        plan["format"] == "theseus-replay-plan-v3"
            && plan["run"]["replay_start"] == "ready_checkpoint",
        "checkpoint replay requires a version-3 ready-checkpoint plan",
    )?;
    for (field, name) in [
        ("metadata", "metadata.json"),
        ("vmstate", "vmstate"),
        ("memory", "memory"),
        ("prelude", "prelude.log"),
    ] {
        let relative = format!("checkpoint/{name}");
        let member = &plan["checkpoint"][field];
        require(
            member["path"] == relative,
            "checkpoint inventory must use fixed bundle-local paths",
        )?;
        let indexed = files
            .get(&format!("container/run/{relative}"))
            .ok_or_else(|| {
                EvidenceError("runtime validation omitted a locked checkpoint member".into())
            })?;
        require(
            member["sha256"] == indexed.sha256,
            "checkpoint identity differs from retained bytes",
        )?;
    }
    let metadata: serde_json::Value = parse_json_bytes(
        retained
            .get("container/run/checkpoint/metadata.json")
            .ok_or_else(|| EvidenceError("checkpoint metadata was not retained".into()))?,
        "checkpoint metadata",
    )?;
    require(
        metadata["format"] == "theseus-checkpoint-v1" && metadata["architecture"] == architecture,
        "checkpoint format or architecture differs from native validation",
    )?;
    let config = &metadata["machine_config"];
    let run = &plan["run"];
    require(
        config["vcpu_count"] == run["vcpu_count"]
            && config["mem_size_mib"] == run["mem_size_mib"]
            && !run["virtual_time"].is_null()
            && config["virtual_time"] == run["virtual_time"]
            && if run["entropy_device"] == false {
                metadata["entropy"].is_null()
            } else {
                metadata["entropy"]["seed"] == run["seed"]
            },
        "checkpoint configuration differs from its replay plan",
    )?;
    for (field, name) in [("snapshot", "vmstate"), ("memory", "memory")] {
        let indexed = &files[&format!("container/run/checkpoint/{name}")];
        require(
            metadata[field]["sha256"] == indexed.sha256
                && metadata[field]["bytes"] == indexed.bytes,
            "checkpoint metadata does not bind its state and RAM",
        )?;
    }
    let memory_bytes = run["mem_size_mib"]
        .as_u64()
        .and_then(|mib| mib.checked_mul(1024 * 1024));
    require(
        memory_bytes.is_some() && metadata["memory"]["bytes"].as_u64() == memory_bytes,
        "checkpoint RAM length differs from its machine configuration",
    )?;
    let prefix: Vec<String> = serde_json::from_value(metadata["execution"]["trace"].clone())
        .map_err(|error| EvidenceError(format!("invalid checkpoint execution prefix: {error}")))?;
    require(
        execution.start.as_ref().is_some_and(|start| {
            start.kind == "checkpoint"
                && start.checkpoint_sha256 == plan["checkpoint"]["metadata"]["sha256"]
                && start.inherited_decisions == prefix.len() as u64
        }) && prefix.len() < execution.machine_execution_trace.len()
            && execution.machine_execution_trace.starts_with(&prefix),
        "execution origin or inherited prefix differs from its retained checkpoint",
    )
}

fn verify_replay(retained: &BTreeMap<String, Vec<u8>>) -> Result<(), EvidenceError> {
    let topology: serde_json::Value = parse_json_bytes(
        &retained["evidence/replay/topology-result.json"],
        "replay topology",
    )?;
    let actions = topology["actions"]
        .as_array()
        .ok_or_else(|| EvidenceError("replay topology has no actions".to_owned()))?;
    for kind in ["partition", "heal"] {
        require(
            actions.iter().any(|action| action["kind"] == kind),
            &format!("replay topology did not apply {kind}"),
        )?;
    }
    let services = ["counter", "writer-a", "writer-b"]
        .iter()
        .map(|service| {
            parse_json_bytes::<serde_json::Value>(
                &retained[&format!("evidence/replay/services/{service}/result.json")],
                "replay service result",
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    require(
        services.iter().all(|result| result["status"] == "passed"),
        "one or more replay services did not pass",
    )?;
    let dropped = services.iter().any(|result| {
        result["network_traffic"]
            .as_object()
            .is_some_and(|networks| {
                networks
                    .values()
                    .any(|network| network["dropped"].as_u64().is_some_and(|count| count > 0))
            })
    });
    require(dropped, "replay has no dropped network frame")?;
    let writer = String::from_utf8_lossy(&retained["evidence/replay/services/writer-a/serial.log"]);
    require(
        writer.contains(r#""network":"recovered""#),
        "replay has no successful recovery probe",
    )?;
    let counter = String::from_utf8_lossy(&retained["evidence/replay/services/counter/serial.log"]);
    require(
        counter.contains(r#""value":1"#),
        "replay does not reproduce the lost update",
    )
}

fn checkpoint_bundle_json(name: &str) -> bool {
    matches!(
        Path::new(name).file_name().and_then(|name| name.to_str()),
        Some("replay-plan.json" | "metadata.json" | "result.json" | "campaign-result.json")
    )
}

fn verify_archived_checkpoint_if_present(
    scope: &str,
    files: &BTreeMap<String, ProofFile>,
    retained: &BTreeMap<String, Vec<u8>>,
    architecture: &str,
) -> Result<(), EvidenceError> {
    let plan_path = Path::new(scope).join("replay-plan.json");
    let Some(bytes) = retained.get(
        plan_path
            .to_str()
            .ok_or_else(|| EvidenceError("invalid bundle scope".into()))?,
    ) else {
        let campaign_path = Path::new(scope).join("campaign-result.json");
        require(
            !retained
                .get(campaign_path.to_str().unwrap())
                .is_some_and(|bytes| {
                    serde_json::from_slice::<serde_json::Value>(bytes)
                        .is_ok_and(|campaign| !campaign["starting_checkpoint_sha256"].is_null())
                }),
            "checkpoint campaign is missing its archived replay plan",
        )?;
        return Ok(()); // Legacy evidence did not carry portable starting roots.
    };
    let plan: serde_json::Value = parse_json_bytes(bytes, "archived checkpoint replay plan")?;
    if plan["replay_start"] != "ready_checkpoint" && plan["starting_checkpoint"].is_null() {
        let campaign_path = Path::new(scope).join("campaign-result.json");
        require(
            !retained
                .get(campaign_path.to_str().unwrap())
                .is_some_and(|bytes| {
                    serde_json::from_slice::<serde_json::Value>(bytes)
                        .is_ok_and(|campaign| !campaign["starting_checkpoint_sha256"].is_null())
                }),
            "checkpoint campaign cannot be downgraded to fresh boot",
        )?;
        require(
            !retained.iter().any(|(name, bytes)| {
                Path::new(name).starts_with(Path::new(scope).join("services"))
                    && name.ends_with("/result.json")
                    && serde_json::from_slice::<serde_json::Value>(bytes)
                        .is_ok_and(|result| !result["execution_start"].is_null())
            }),
            "checkpoint evidence cannot be downgraded to fresh boot",
        )?;
        return Ok(());
    }
    let inventory = files
        .iter()
        .map(|(name, file)| (name.clone(), (file.sha256.clone(), file.bytes)))
        .collect();
    let summary = crate::topology_evidence::verify_archived_bundle(scope, &inventory, retained)
        .map_err(EvidenceError)?;
    require(
        summary.architecture == architecture,
        "archived topology checkpoint architecture differs from its index",
    )
}

fn verify_runtime_validation(
    path: &Path,
    architecture: &str,
    source_commit: &str,
    runtime_tag: &str,
    certificate_bytes: &[u8],
) -> Result<(), EvidenceError> {
    let required = BTreeSet::from([
        "evidence.json",
        "container/plan.json",
        "container/run/replay-plan.json",
        "container/run/result.json",
        "container/run/serial.log",
        "container/replay.log",
        "container/source/Dockerfile",
        "container/source/theseus.toml",
        "coverage/plan.json",
        "coverage/campaign/campaign-result.json",
        "coverage/campaign/replay-plan.json",
        "coverage/report/report.md",
        "coverage/rerun/campaign-result.json",
        "coverage/comparison.json",
        "coverage/evaluation.json",
        "coverage/evaluation/theseus-evaluation.toml",
        "coverage/evaluation/theseus-evaluation.lock",
        "coverage/source/compose.yaml",
        "coverage/source/service/main.c",
        "coverage/source/service/theseus.toml",
        "schedule-search/plan.json",
        "schedule-search/campaign/campaign-result.json",
        "schedule-search/campaign/replay-plan.json",
        "schedule-search/report/report.md",
        "schedule-search/minimized/minimization.json",
        "schedule-search/minimized/replay-plan.json",
        "schedule-search/minimized/services/ledger/result.json",
        "schedule-search/minimized/topology-result.json",
        "schedule-search/rerun/replay-plan.json",
        "schedule-search/rerun/services/ledger/result.json",
        "schedule-search/rerun/topology-result.json",
        "schedule-search/source/compose.yaml",
        "schedule-search/source/service/main.c",
        "schedule-search/source/service/theseus.toml",
        "pthread-sync/plan.json",
        "pthread-sync/campaign/campaign-result.json",
        "pthread-sync/campaign/replay-plan.json",
        "pthread-sync/report/report.md",
        "pthread-sync/rerun/campaign-result.json",
        "pthread-sync/source/compose.yaml",
        "pthread-sync/source/service/main.c",
        "pthread-sync/source/service/theseus.toml",
    ]);
    let strict_required = BTreeSet::from([
        "strict-execution/plan.json",
        "strict-execution/campaign/campaign-result.json",
        "strict-execution/campaign/replay-plan.json",
        "strict-execution/report/report.md",
        "strict-execution/rerun/campaign-result.json",
        "strict-execution/comparison.json",
        "strict-execution/source/.dockerignore",
        "strict-execution/source/Dockerfile",
        "strict-execution/source/compose.yaml",
        "strict-execution/source/api/theseus.toml",
    ]);
    let api_required = BTreeSet::from([
        "container/run/execution.json",
        "container/rerun/execution.json",
        "container/rerun/result.json",
        "container/rerun/serial.log",
    ]);
    let file = fs::File::open(path)
        .map_err(|error| EvidenceError(format!("cannot read {}: {error}", path.display())))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    let mut files = BTreeMap::new();
    let mut retained = BTreeMap::<String, Vec<u8>>::new();
    for entry in archive
        .entries()
        .map_err(|error| EvidenceError(format!("cannot open {}: {error}", path.display())))?
    {
        let mut entry = entry
            .map_err(|error| EvidenceError(format!("cannot read {}: {error}", path.display())))?;
        let entry_path = entry
            .path()
            .map_err(|error| EvidenceError(format!("invalid archive path: {error}")))?;
        require_safe_archive_path(&entry_path)?;
        let relative = entry_path.strip_prefix("validation").map_err(|_| {
            EvidenceError("runtime validation archive root must be validation".to_owned())
        })?;
        if relative.as_os_str().is_empty() || entry.header().entry_type().is_dir() {
            continue;
        }
        require(
            entry.header().entry_type().is_file(),
            "runtime validation archive may contain only files and directories",
        )?;
        let name = relative
            .to_str()
            .ok_or_else(|| EvidenceError("runtime validation path is not UTF-8".to_owned()))?
            .to_owned();
        let mut bytes = Vec::new();
        let mut hasher = Sha256::new();
        if name == "container/run/checkpoint/metadata.json" {
            require(
                entry.header().size().unwrap_or(u64::MAX) <= 128 * 1024 * 1024,
                "checkpoint metadata exceeds 128 MiB",
            )?;
        }
        if required.contains(name.as_str())
            || strict_required.contains(name.as_str())
            || api_required.contains(name.as_str())
            || name == "container/run/checkpoint/metadata.json"
            || (name.starts_with("fixed-plan/") && name.ends_with(".json"))
            || checkpoint_bundle_json(&name)
        {
            require(
                entry.header().size().unwrap_or(u64::MAX) <= 128 * 1024 * 1024,
                "retained validation JSON exceeds 128 MiB",
            )?;
            entry.read_to_end(&mut bytes).map_err(|error| {
                EvidenceError(format!("cannot read archive member {name}: {error}"))
            })?;
            hasher.update(&bytes);
            require(
                retained
                    .values()
                    .map(Vec::len)
                    .sum::<usize>()
                    .saturating_add(bytes.len())
                    <= 512 * 1024 * 1024,
                "retained validation JSON exceeds 512 MiB total",
            )?;
            retained.insert(name.clone(), bytes);
        } else {
            std::io::copy(&mut entry, &mut hasher).map_err(|error| {
                EvidenceError(format!("cannot hash archive member {name}: {error}"))
            })?;
        }
        let size = entry.header().size().unwrap_or(0);
        require(
            files
                .insert(
                    name,
                    ProofFile {
                        sha256: format!("{:x}", hasher.finalize()),
                        bytes: size,
                    },
                )
                .is_none(),
            "runtime validation archive contains duplicate paths",
        )?;
    }
    require(
        required.iter().all(|name| retained.contains_key(*name)),
        "runtime validation archive is missing required evidence",
    )?;
    let proof: RuntimeValidationProof =
        parse_json_bytes(&retained["evidence.json"], "runtime validation proof")?;
    let certificate: serde_json::Value =
        parse_json_bytes(certificate_bytes, "runtime certificate")?;
    if certificate["format"] == CERTIFICATE_FORMAT_V5 {
        verify_fixed_plan(&files, &retained, architecture, certificate_bytes)?;
    }
    for scope in [
        "coverage/campaign",
        "coverage/rerun",
        "schedule-search/campaign",
        "schedule-search/minimized",
        "schedule-search/rerun",
        "pthread-sync/campaign",
        "pthread-sync/rerun",
        "strict-execution/campaign",
        "strict-execution/rerun",
    ] {
        verify_archived_checkpoint_if_present(scope, &files, &retained, architecture)?;
    }
    require(
        matches!(
            proof.format.as_str(),
            VALIDATION_FORMAT_V1
                | VALIDATION_FORMAT_V2
                | VALIDATION_FORMAT_V3
                | VALIDATION_FORMAT_V4
                | VALIDATION_FORMAT_V5
        ),
        "unsupported runtime validation format",
    )?;
    require(
        proof.architecture == architecture && proof.source_commit == source_commit,
        "runtime validation identity differs from its index",
    )?;
    require(
        proof.runtime.tag == format!("{runtime_tag}-{architecture}"),
        "runtime validation tag differs from its index",
    )?;
    require_digest_reference(&proof.runtime.image)?;
    require(
        !proof.host.kernel_release.is_empty() && proof.host.kvm_api_version == 12,
        "runtime validation was not produced by KVM API version 12",
    )?;
    let expected_scenarios = if proof.format != VALIDATION_FORMAT_V1 {
        require(
            strict_required
                .iter()
                .all(|name| retained.contains_key(*name)),
            "runtime validation archive is missing strict execution evidence",
        )?;
        vec![
            "container",
            "coverage",
            "schedule-search",
            "pthread-sync",
            "strict-execution",
        ]
    } else {
        vec!["container", "coverage", "schedule-search", "pthread-sync"]
    };
    require(
        proof.scenarios
            == expected_scenarios
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        "runtime validation does not contain the required scenarios",
    )?;
    files.remove("evidence.json");
    require(
        proof.files.len() == files.len()
            && proof.files.iter().all(|(name, expected)| {
                files.get(name).is_some_and(|actual| {
                    expected.sha256 == actual.sha256 && expected.bytes == actual.bytes
                })
            }),
        "runtime validation inventory does not match its contents",
    )?;
    let container_plan: serde_json::Value =
        parse_json_bytes(&retained["container/plan.json"], "container plan")?;
    require(
        container_plan["format"] == "theseus-run-plan-v1",
        "container/plan.json is not a run plan",
    )?;
    for name in [
        "coverage/plan.json",
        "schedule-search/plan.json",
        "pthread-sync/plan.json",
    ] {
        let plan: serde_json::Value = parse_json_bytes(&retained[name], name)?;
        require(
            plan["format"] == "theseus-compose-plan-v1",
            &format!("{name} is not a Compose plan"),
        )?;
    }
    if proof.format != VALIDATION_FORMAT_V1 {
        let plan: serde_json::Value = parse_json_bytes(
            &retained["strict-execution/plan.json"],
            "strict execution plan",
        )?;
        require(
            plan["format"] == "theseus-compose-plan-v1",
            "strict-execution/plan.json is not a Compose plan",
        )?;
    }
    let container: serde_json::Value =
        parse_json_bytes(&retained["container/run/result.json"], "container result")?;
    require(
        container["status"] == "passed",
        "container scenario did not pass",
    )?;
    require(
        String::from_utf8_lossy(&retained["container/run/serial.log"])
            .contains("THES:HTTP:operation:read_health:PASS"),
        "container scenario did not retain its health operation",
    )?;
    require(
        String::from_utf8_lossy(&retained["container/replay.log"]).contains("replay passed"),
        "container scenario did not retain a successful replay",
    )?;
    let replay_plan: serde_json::Value = parse_json_bytes(
        &retained["container/run/replay-plan.json"],
        "container replay plan",
    )?;
    if proof.format == VALIDATION_FORMAT_V5
        || replay_plan["format"] == "theseus-replay-plan-v2"
        || replay_plan["format"] == "theseus-replay-plan-v3"
        || container["execution_evidence"] == "execution.json"
    {
        require(
            api_required.iter().all(|name| retained.contains_key(*name)),
            "runtime validation is missing API machine-stream replay evidence",
        )?;
        require(
            matches!(
                replay_plan["format"].as_str(),
                Some("theseus-replay-plan-v2" | "theseus-replay-plan-v3")
            ) && container["execution_evidence"] == "execution.json",
            "container run did not require machine-stream capture",
        )?;
        let vcpu_count = replay_plan["run"]["vcpu_count"]
            .as_u64()
            .and_then(|count| u8::try_from(count).ok())
            .ok_or_else(|| EvidenceError("container replay plan has no valid vCPU count".into()))?;
        let original: crate::execution::Evidence = parse_json_bytes(
            &retained["container/run/execution.json"],
            "container execution",
        )?;
        let replay: crate::execution::Evidence = parse_json_bytes(
            &retained["container/rerun/execution.json"],
            "container replay execution",
        )?;
        original.validate(vcpu_count).map_err(EvidenceError)?;
        replay.validate(vcpu_count).map_err(EvidenceError)?;
        verify_api_origin(&replay_plan, &original, &files, &retained, architecture)?;
        let machine_replay = replay_plan["run"]["machine_replay"]
            .as_str()
            .unwrap_or("exact");
        require(
            matches!(machine_replay, "exact" | "host_inputs"),
            "container replay plan has an unsupported machine replay contract",
        )?;
        let execution_matches = if machine_replay == "host_inputs" {
            original.boundary == replay.boundary
                && original.start == replay.start
                && machine_replay_control_trace(&original.machine_execution_trace)
                    == machine_replay_control_trace(&replay.machine_execution_trace)
        } else {
            original == replay
        };
        require(
            original.boundary == "guest_exit"
                && original.replay_error.is_none()
                && replay.replay_error.is_none()
                && !original.machine_execution_trace.is_empty()
                && execution_matches,
            "container API replay did not satisfy its declared machine replay contract through guest exit",
        )?;
        let result: serde_json::Value = parse_json_bytes(
            &retained["container/rerun/result.json"],
            "container replay result",
        )?;
        require(
            result["status"] == "passed"
                && result["execution_evidence"] == "execution.json"
                && result["checks"].as_array().is_some_and(|checks| {
                    checks.iter().any(|check| {
                        check["name"] == "replay_machine_execution" && check["status"] == "passed"
                    })
                }),
            "container API replay has no passing active-replay check",
        )?;
        require(
            String::from_utf8_lossy(&retained["container/rerun/serial.log"])
                .contains("THES:HTTP:operation:read_health:PASS"),
            "container API replay has no successful health operation",
        )?;
    }
    verify_validation_campaign(
        &retained["coverage/campaign/campaign-result.json"],
        "coverage",
        "unique_application_blocks",
        false,
    )?;
    verify_validation_campaign(
        &retained["coverage/rerun/campaign-result.json"],
        "coverage replay",
        "unique_application_blocks",
        true,
    )?;
    let comparison: serde_json::Value =
        parse_json_bytes(&retained["coverage/comparison.json"], "campaign comparison")?;
    require(
        comparison["format"] == "theseus-campaign-comparison-v1" && comparison["status"] == "same",
        "released CLI did not retain an identical campaign comparison",
    )?;
    let evaluation: serde_json::Value =
        parse_json_bytes(&retained["coverage/evaluation.json"], "campaign evaluation")?;
    require(
        evaluation["format"] == "theseus-public-evaluation-v1"
            && evaluation["status"] == "passed"
            && evaluation["workloads"]
                .as_array()
                .is_some_and(|workloads| !workloads.is_empty()),
        "released CLI did not retain a non-empty passing evaluation",
    )?;
    verify_validation_campaign(
        &retained["schedule-search/campaign/campaign-result.json"],
        "schedule search",
        "thread_scheduling_decisions",
        false,
    )?;
    let schedule: serde_json::Value = parse_json_bytes(
        &retained["schedule-search/campaign/campaign-result.json"],
        "schedule search",
    )?;
    require(
        schedule["properties"].as_array().is_some_and(|properties| {
            properties.iter().any(|property| {
                property["name"] == "lost_update_is_unreachable" && property["status"] == "failed"
            })
        }),
        "schedule search did not retain its lost-update counterexample",
    )?;
    let minimization: serde_json::Value = parse_json_bytes(
        &retained["schedule-search/minimized/minimization.json"],
        "schedule minimization",
    )?;
    require(
        minimization["property"] == "lost_update_is_unreachable",
        "schedule minimization names the wrong property",
    )?;
    verify_validation_topology_replay(
        &retained["schedule-search/rerun/replay-plan.json"],
        &retained["schedule-search/rerun/services/ledger/result.json"],
        "schedule replay",
        "counterexample: lost_update_is_unreachable",
    )?;
    verify_validation_campaign(
        &retained["pthread-sync/campaign/campaign-result.json"],
        "pthread synchronization",
        "thread_synchronization_events",
        false,
    )?;
    verify_validation_campaign(
        &retained["pthread-sync/rerun/campaign-result.json"],
        "pthread synchronization replay",
        "thread_synchronization_events",
        true,
    )?;
    if proof.format != VALIDATION_FORMAT_V1 {
        verify_validation_campaign(
            &retained["strict-execution/campaign/campaign-result.json"],
            "strict execution",
            "execution_decisions",
            false,
        )?;
        verify_campaign_execution_ledgers(
            &retained["strict-execution/campaign/campaign-result.json"],
            "strict execution",
        )?;
        if matches!(
            proof.format.as_str(),
            VALIDATION_FORMAT_V3 | VALIDATION_FORMAT_V4 | VALIDATION_FORMAT_V5
        ) {
            verify_campaign_machine_execution_ledgers(
                &retained["strict-execution/campaign/campaign-result.json"],
                "strict execution",
            )?;
        }
        if matches!(
            proof.format.as_str(),
            VALIDATION_FORMAT_V4 | VALIDATION_FORMAT_V5
        ) {
            verify_campaign_machine_execution_traces(
                &retained["strict-execution/campaign/campaign-result.json"],
                "strict execution",
            )?;
        }
        verify_validation_campaign(
            &retained["strict-execution/rerun/campaign-result.json"],
            "strict execution replay",
            "execution_decisions",
            true,
        )?;
        verify_campaign_execution_ledgers(
            &retained["strict-execution/rerun/campaign-result.json"],
            "strict execution replay",
        )?;
        if matches!(
            proof.format.as_str(),
            VALIDATION_FORMAT_V3 | VALIDATION_FORMAT_V4 | VALIDATION_FORMAT_V5
        ) {
            verify_campaign_machine_execution_ledgers(
                &retained["strict-execution/rerun/campaign-result.json"],
                "strict execution replay",
            )?;
        }
        if matches!(
            proof.format.as_str(),
            VALIDATION_FORMAT_V4 | VALIDATION_FORMAT_V5
        ) {
            verify_campaign_machine_execution_traces(
                &retained["strict-execution/rerun/campaign-result.json"],
                "strict execution replay",
            )?;
        }
        let comparison: serde_json::Value = parse_json_bytes(
            &retained["strict-execution/comparison.json"],
            "strict execution comparison",
        )?;
        require(
            comparison["format"] == "theseus-campaign-comparison-v1"
                && comparison["status"] == "same",
            "strict execution comparison did not retain an identical replay",
        )?;
    }
    for name in [
        "coverage/report/report.md",
        "schedule-search/report/report.md",
        "pthread-sync/report/report.md",
    ] {
        require(!retained[name].is_empty(), &format!("{name} is empty"))?;
    }
    if proof.format != VALIDATION_FORMAT_V1 {
        require(
            String::from_utf8_lossy(&retained["strict-execution/report/report.md"])
                .contains("Execution ledger"),
            "strict execution report does not explain the execution ledger",
        )?;
    }
    Ok(())
}

fn machine_replay_control_trace(trace: &[String]) -> Vec<&str> {
    trace
        .iter()
        .filter(|record| record.starts_with("host:"))
        .map(String::as_str)
        .collect()
}

/// Bind a checkpoint certificate to the entire retained first/replay witness.
/// RAM/context are streamed into the inventory; only inspectable JSON is read.
fn verify_fixed_plan(
    files: &BTreeMap<String, ProofFile>,
    retained: &BTreeMap<String, Vec<u8>>,
    architecture: &str,
    certificate_bytes: &[u8],
) -> Result<(), EvidenceError> {
    let get = |name: &str| {
        retained
            .get(name)
            .ok_or_else(|| EvidenceError(format!("missing fixed-plan evidence: {name}")))
    };
    require(
        get("fixed-plan/certificate.json")? == certificate_bytes,
        "retained fixed-plan certificate differs from its indexed asset",
    )?;
    let first: serde_json::Value =
        parse_json_bytes(get("fixed-plan/first/replay-plan.json")?, "fixed-plan plan")?;
    let certificate: serde_json::Value =
        parse_json_bytes(certificate_bytes, "fixed-plan certificate")?;
    require(
        certificate["source"]["plan_contents"]
            .as_str()
            .is_some_and(|contents| {
                contents.as_bytes() == get("fixed-plan/first/replay-plan.json").unwrap()
            }),
        "fixed-plan witness differs from the certificate's exact embedded plan",
    )?;
    verify_fixed_plan_artifacts(&first, "fixed-plan/first", files)?;
    let metadata_path = "fixed-plan/first/checkpoint/starting-state/metadata.json";
    let metadata: serde_json::Value =
        parse_json_bytes(get(metadata_path)?, "topology checkpoint metadata")?;
    let identity = first["starting_checkpoint"]["sha256"]
        .as_str()
        .ok_or_else(|| EvidenceError("fixed plan lacks its checkpoint identity".to_owned()))?;
    require(
        files
            .get(metadata_path)
            .is_some_and(|file| file.sha256 == identity)
            && first["starting_checkpoint"]["path"] == "checkpoint/starting-state/metadata.json"
            && first["replay_start"] == "ready_checkpoint"
            && metadata["format"] == "theseus-topology-checkpoint-v1"
            && metadata["architecture"] == architecture,
        "fixed-plan checkpoint identity, path, format, or architecture differs",
    )?;
    let services = first["services"]
        .as_object()
        .ok_or_else(|| EvidenceError("fixed plan has no services".to_owned()))?;
    require(
        !services.is_empty()
            && metadata["services"]
                .as_object()
                .is_some_and(|members| members.keys().eq(services.keys()))
            && metadata["execution_prefixes"]
                .as_object()
                .is_some_and(|prefixes| prefixes.keys().eq(services.keys())),
        "fixed-plan checkpoint service or prefix set differs",
    )?;
    let member =
        |name: &str, expected: &serde_json::Value, maximum: u64| -> Result<(), EvidenceError> {
            let file = files.get(name).ok_or_else(|| {
                EvidenceError(format!("missing fixed-plan checkpoint member: {name}"))
            })?;
            require(
                expected["sha256"].as_str() == Some(file.sha256.as_str())
                    && expected["bytes"].as_u64() == Some(file.bytes)
                    && file.bytes <= maximum,
                "fixed-plan checkpoint member hash, size, or bound differs",
            )
        };
    member(
        "fixed-plan/first/checkpoint/starting-state/context.bin",
        &metadata["context"],
        128 * 1024 * 1024,
    )?;
    for (name, profile) in services {
        require(
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']),
            "unsafe fixed-plan service name",
        )?;
        let memory = &metadata["services"][name]["memory"];
        let memory_mib = profile["run"]["run"]["mem_size_mib"].as_u64().unwrap_or(0);
        require(
            (1..=65536).contains(&memory_mib)
                && memory["bytes"].as_u64() == Some(memory_mib * 1024 * 1024),
            "fixed-plan RAM length differs from its VM configuration",
        )?;
        member(
            &format!("fixed-plan/first/checkpoint/starting-state/{name}/memory"),
            memory,
            64 * 1024 * 1024 * 1024,
        )?;
        member(
            &format!("fixed-plan/first/checkpoint/starting-state/{name}/vmstate"),
            &metadata["services"][name]["vmstate"],
            128 * 1024 * 1024,
        )?;
        let prefix = metadata["execution_prefixes"][name]
            .as_array()
            .ok_or_else(|| EvidenceError("missing fixed-plan execution prefix".to_owned()))?;
        require(
            !prefix.is_empty()
                && first["checkpoint_prefixes"][name].as_u64() == Some(prefix.len() as u64),
            "fixed-plan ancestry count differs",
        )?;
        let original: serde_json::Value = parse_json_bytes(
            get(&format!("fixed-plan/first/services/{name}/result.json"))?,
            "first service result",
        )?;
        let replay: serde_json::Value = parse_json_bytes(
            get(&format!("fixed-plan/replay/services/{name}/result.json"))?,
            "replayed service result",
        )?;
        for result in [&original, &replay] {
            require(
                result["status"] == "passed"
                    && result["error"].is_null()
                    && result["execution_start"]["kind"] == "topology_checkpoint"
                    && result["execution_start"]["checkpoint_sha256"] == identity
                    && result["execution_start"]["inherited_decisions"].as_u64()
                        == Some(prefix.len() as u64),
                "fixed-plan result failed or has a different checkpoint origin",
            )?;
            let trace = result["machine_execution_trace"]
                .as_array()
                .ok_or_else(|| EvidenceError("missing fixed-plan machine trace".to_owned()))?;
            require(
                trace.len() > prefix.len() && trace.starts_with(prefix),
                "fixed-plan stream lacks its exact ancestry and nonempty suffix",
            )?;
            let evidence: crate::execution::Evidence = serde_json::from_value(serde_json::json!({
                "format": "theseus-execution-v1", "boundary": "guest_exit", "replay_error": null,
                "execution_ledgers": result["execution_ledgers"], "machine_execution_ledger": result["machine_execution_ledger"],
                "machine_execution_trace": result["machine_execution_trace"]
            })).map_err(|error| EvidenceError(error.to_string()))?;
            let cpus = profile["run"]["run"]["vcpu_count"]
                .as_u64()
                .filter(|count| (1..=32).contains(count))
                .ok_or_else(|| EvidenceError("invalid fixed-plan CPU count".to_owned()))?
                as u8;
            evidence.validate(cpus).map_err(EvidenceError)?;
        }
        for field in [
            "machine_execution_trace",
            "execution_ledgers",
            "machine_execution_ledger",
            "serial_sha256",
            "storage_sha256",
            "network_traffic",
            "entropy_probe_sha256",
            "virtual_time_ns",
        ] {
            require(
                original[field] == replay[field],
                &format!("fixed-plan replay differs at {name}.{field}"),
            )?;
        }
        require(
            replay["checks"].as_array().is_some_and(|checks| {
                checks.iter().any(|check| {
                    check["name"] == "replay_machine_execution_trace" && check["status"] == "passed"
                })
            }),
            "fixed-plan witness lacks passing active replay admission",
        )?;
        for execution in ["first", "replay"] {
            let serial = files
                .get(&format!(
                    "fixed-plan/{execution}/services/{name}/serial.log"
                ))
                .ok_or_else(|| EvidenceError("missing fixed-plan UART output".to_owned()))?;
            require(
                original["serial_sha256"][0].as_str() == Some(serial.sha256.as_str()),
                "fixed-plan UART digest differs from retained bytes",
            )?;
        }
    }
    Ok(())
}

fn verify_fixed_plan_artifacts(
    value: &serde_json::Value,
    base: &str,
    files: &BTreeMap<String, ProofFile>,
) -> Result<(), EvidenceError> {
    match value {
        serde_json::Value::Object(map) => {
            if let (Some(path), Some(digest)) = (map.get("path"), map.get("sha256")) {
                let path = path
                    .as_str()
                    .ok_or_else(|| EvidenceError("invalid fixed-plan artifact path".to_owned()))?;
                let mut parts: Vec<&str> = base.split('/').collect();
                for component in Path::new(path).components() {
                    match component {
                        Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| {
                            EvidenceError("non-UTF8 fixed-plan artifact".to_owned())
                        })?),
                        Component::CurDir => {}
                        Component::ParentDir if parts.len() > 1 => {
                            parts.pop();
                        }
                        _ => {
                            return Err(EvidenceError(
                                "fixed-plan artifact escapes the retained witness".to_owned(),
                            ))
                        }
                    }
                }
                let name = parts.join("/");
                require(
                    files
                        .get(&name)
                        .is_some_and(|file| digest.as_str() == Some(file.sha256.as_str())),
                    &format!("fixed-plan locked artifact is absent or changed: {name}"),
                )?;
            }
            for child in map.values() {
                verify_fixed_plan_artifacts(child, base, files)?;
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                verify_fixed_plan_artifacts(child, base, files)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn verify_campaign_execution_ledgers(bytes: &[u8], scenario: &str) -> Result<(), EvidenceError> {
    let value: serde_json::Value = parse_json_bytes(bytes, scenario)?;
    let valid = value["runs"].as_array().is_some_and(|runs| {
        !runs.is_empty()
            && runs.iter().all(|run| {
                run["execution_ledgers"]
                    .as_object()
                    .is_some_and(|services| {
                        !services.is_empty()
                            && services.values().all(|ledgers| {
                                ledgers.as_array().is_some_and(|ledgers| {
                                    !ledgers.is_empty()
                                        && ledgers.iter().all(valid_json_execution_ledger)
                                })
                            })
                    })
            })
    });
    require(
        valid,
        &format!("{scenario} has no valid ordered KVM execution ledger"),
    )
}

fn verify_campaign_machine_execution_ledgers(
    bytes: &[u8],
    scenario: &str,
) -> Result<(), EvidenceError> {
    let value: serde_json::Value = parse_json_bytes(bytes, scenario)?;
    let valid = value["runs"].as_array().is_some_and(|runs| {
        !runs.is_empty()
            && runs.iter().all(|run| {
                run["machine_execution_ledgers"]
                    .as_object()
                    .is_some_and(|services| {
                        !services.is_empty() && services.values().all(valid_json_execution_ledger)
                    })
            })
    });
    require(
        valid,
        &format!("{scenario} has no valid machine-wide execution stream"),
    )
}

fn verify_campaign_machine_execution_traces(
    bytes: &[u8],
    scenario: &str,
) -> Result<(), EvidenceError> {
    let value: serde_json::Value = parse_json_bytes(bytes, scenario)?;
    let valid = value["runs"].as_array().is_some_and(|runs| {
        !runs.is_empty()
            && runs.iter().all(|run| {
                let Some(traces) = run["machine_execution_traces"].as_object() else {
                    return false;
                };
                let Some(ledgers) = run["machine_execution_ledgers"].as_object() else {
                    return false;
                };
                !traces.is_empty()
                    && traces.keys().eq(ledgers.keys())
                    && traces.iter().all(|(service, trace)| {
                        trace.as_array().is_some_and(|records| {
                            !records.is_empty()
                                && records.iter().all(|record| {
                                    record.as_str().is_some_and(valid_machine_execution_record)
                                })
                                && ledgers
                                    .get(service)
                                    .and_then(|ledger| ledger["decisions"].as_u64())
                                    == u64::try_from(records.len()).ok()
                        })
                    })
            })
    });
    require(
        valid,
        &format!("{scenario} has no valid active machine execution replay trace"),
    )
}

fn valid_json_execution_ledger(ledger: &serde_json::Value) -> bool {
    ledger["decisions"].as_u64().is_some_and(|count| count > 0)
        && ledger["sha256"].as_str().is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        && ledger["tail"]
            .as_array()
            .is_some_and(|tail| !tail.is_empty())
}

pub(crate) fn valid_machine_execution_record(record: &str) -> bool {
    if let Some(effect) = record.strip_prefix("host:") {
        if effect == "ctrl_alt_del" {
            return true;
        }
        if let Some(byte) = effect.strip_prefix("control_event:") {
            return valid_lowercase_hex(byte) && byte.len() == 2;
        }
        if let Some(serial) = effect.strip_prefix("serial_input:") {
            let Some((length_text, bytes)) = serial.split_once(':') else {
                return false;
            };
            let Ok(length) = length_text.parse::<usize>() else {
                return false;
            };
            return length > 0
                && length_text == length.to_string()
                && bytes.len() == length.saturating_mul(2)
                && valid_lowercase_hex(bytes);
        }
        let Some(delta_text) = effect.strip_prefix("virtual_time_jump:") else {
            return false;
        };
        return delta_text
            .parse::<u64>()
            .is_ok_and(|delta| delta > 0 && delta_text == delta.to_string());
    }
    record
        .strip_prefix("vcpu:")
        .and_then(|record| record.split_once(':'))
        .is_some_and(|(vcpu, effect)| {
            vcpu.parse::<u8>()
                .is_ok_and(|id| vcpu == id.to_string() && valid_machine_vcpu_effect(effect))
        })
}

fn valid_machine_vcpu_effect(effect: &str) -> bool {
    let Some(interrupt) = effect.strip_prefix("interrupt:") else {
        return !effect.is_empty();
    };
    let Some((source, gsi_text)) = interrupt.split_once(':') else {
        return false;
    };
    let Ok(gsi) = gsi_text.parse::<u32>() else {
        return false;
    };
    matches!(
        source,
        "serial" | "virtio-mmio" | "virtio-msix" | "vmgenid" | "vmclock" | "i8042"
    ) && gsi_text == gsi.to_string()
}

fn valid_lowercase_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|digit| digit.is_ascii_hexdigit() && !digit.is_ascii_uppercase())
}

fn verify_validation_campaign(
    bytes: &[u8],
    scenario: &str,
    signal: &str,
    replay: bool,
) -> Result<(), EvidenceError> {
    let value: serde_json::Value = parse_json_bytes(bytes, scenario)?;
    require(
        value["format"] == "theseus-compose-campaign-result-v1",
        &format!("{scenario} has the wrong campaign format"),
    )?;
    require(
        value[signal].as_u64().is_some_and(|count| count > 0),
        &format!("{scenario} retained no {signal}"),
    )?;
    if replay {
        require(
            value["replay_verification"]["status"] == "passed",
            &format!("{scenario} did not pass replay verification"),
        )?;
    }
    Ok(())
}

fn verify_validation_topology_replay(
    plan_bytes: &[u8],
    result_bytes: &[u8],
    scenario: &str,
    counterexample: &str,
) -> Result<(), EvidenceError> {
    let plan: serde_json::Value = parse_json_bytes(plan_bytes, scenario)?;
    let services = plan["services"]
        .as_object()
        .filter(|services| !services.is_empty())
        .ok_or_else(|| EvidenceError(format!("{scenario} has no services")))?;
    require(
        plan["format"] == "theseus-compose-plan-v1"
            && plan["machine_replay"] == "host_inputs"
            && plan["campaign"].is_null(),
        &format!("{scenario} is not a fixed host-input replay"),
    )?;
    let vcpu_count = services
        .values()
        .next()
        .and_then(|service| service["run"]["run"]["vcpu_count"].as_u64())
        .filter(|count| (1..=32).contains(count))
        .ok_or_else(|| EvidenceError(format!("{scenario} has an invalid vCPU count")))?
        as u8;
    let result: serde_json::Value = parse_json_bytes(result_bytes, scenario)?;
    let passed_check = |name: &str| {
        result["checks"].as_array().is_some_and(|checks| {
            checks
                .iter()
                .any(|check| check["name"] == name && check["status"] == "passed")
        })
    };
    require(
        result["status"] == "passed"
            && result["error"].is_null()
            && passed_check("campaign_checkpoint")
            && passed_check(counterexample)
            && passed_check("replay_machine_execution_trace"),
        &format!("{scenario} did not reproduce and actively govern its counterexample"),
    )?;
    require(
        result["machine_execution_trace"]
            .as_array()
            .is_some_and(|trace| !trace.is_empty()),
        &format!("{scenario} retained no machine execution trace"),
    )?;
    let evidence: crate::execution::Evidence = serde_json::from_value(serde_json::json!({
        "format": "theseus-execution-v1",
        "boundary": "pause",
        "execution_ledgers": result["execution_ledgers"],
        "machine_execution_ledger": result["machine_execution_ledger"],
        "machine_execution_trace": result["machine_execution_trace"],
        "replay_error": null
    }))
    .map_err(|error| EvidenceError(format!("invalid {scenario} execution evidence: {error}")))?;
    evidence.validate(vcpu_count).map_err(EvidenceError)
}

fn require_safe_archive_path(path: &Path) -> Result<(), EvidenceError> {
    require(
        !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "evidence archive contains an unsafe path",
    )
}

fn require_digest_reference(reference: &str) -> Result<(), EvidenceError> {
    let Some((name, digest)) = reference.rsplit_once("@sha256:") else {
        return Err(EvidenceError(
            "runtime image is not digest-pinned".to_owned(),
        ));
    };
    require(!name.is_empty(), "runtime image name is empty")?;
    require_sha256(digest, "runtime image digest")
}

fn require_commit(commit: &str) -> Result<(), EvidenceError> {
    require(
        commit.len() == 40
            && commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "native evidence source commit is not a full lowercase Git SHA",
    )
}

fn require_sha256(digest: &str, subject: &str) -> Result<(), EvidenceError> {
    require(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        &format!("{subject} is not a lowercase SHA-256 digest"),
    )
}

fn read(path: &Path) -> Result<Vec<u8>, EvidenceError> {
    fs::read(path)
        .map_err(|error| EvidenceError(format!("cannot read {}: {error}", path.display())))
}

fn parse_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    bytes: &[u8],
) -> Result<T, EvidenceError> {
    serde_json::from_slice(bytes)
        .map_err(|error| EvidenceError(format!("cannot parse {}: {error}", path.display())))
}

fn parse_json_bytes<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    subject: &str,
) -> Result<T, EvidenceError> {
    serde_json::from_slice(bytes)
        .map_err(|error| EvidenceError(format!("cannot parse {subject}: {error}")))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn require(condition: bool, message: &str) -> Result<(), EvidenceError> {
    if condition {
        Ok(())
    } else {
        Err(EvidenceError(message.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_checkpoint_evidence_binds_members_origin_and_prefix() {
        let mut plan = serde_json::json!({"format":"theseus-replay-plan-v3",
            "run":{"replay_start":"ready_checkpoint", "seed":42, "vcpu_count":1, "mem_size_mib":1,
                "virtual_time":{"tick_ns":1000000,"exits_per_tick":10}}, "checkpoint":{}});
        let mut files = BTreeMap::new();
        for (field, name, bytes) in [
            ("metadata", "metadata.json", 100),
            ("vmstate", "vmstate", 20),
            ("memory", "memory", 1024 * 1024),
            ("prelude", "prelude.log", 10),
        ] {
            plan["checkpoint"][field] =
                serde_json::json!({"path":format!("checkpoint/{name}"),"sha256":"a".repeat(64)});
            files.insert(
                format!("container/run/checkpoint/{name}"),
                ProofFile {
                    bytes,
                    sha256: "a".repeat(64),
                },
            );
        }
        let metadata = serde_json::json!({"format":"theseus-checkpoint-v1","architecture":"amd64",
            "machine_config":{"vcpu_count":1,"mem_size_mib":1,"virtual_time":plan["run"]["virtual_time"]},
            "entropy":{"seed":42},"snapshot":{"sha256":"a".repeat(64),"bytes":20},
            "memory":{"sha256":"a".repeat(64),"bytes":1024*1024},
            "execution":{"trace":["vcpu:0:pio_write:0x3f8:1:41"]}});
        let mut retained = BTreeMap::from([(
            "container/run/checkpoint/metadata.json".into(),
            serde_json::to_vec(&metadata).unwrap(),
        )]);
        // Origin validation is separate from digest/terminal validation; the
        // archive verifier invokes both on original and replay evidence.
        let mut execution: crate::execution::Evidence = serde_json::from_value(serde_json::json!({
            "format":"theseus-execution-v1","boundary":"guest_exit","replay_error":null,
            "execution_ledgers":[],"machine_execution_ledger":{"decisions":0,"sha256":"","tail":[]},
            "machine_execution_trace":["vcpu:0:pio_write:0x3f8:1:41","vcpu:0:pio_write:0x64:1:fe"],
            "start":{"kind":"checkpoint","checkpoint_sha256":"a".repeat(64),"inherited_decisions":1}})).unwrap();
        verify_api_origin(&plan, &execution, &files, &retained, "amd64").unwrap();
        execution.start.as_mut().unwrap().inherited_decisions = 0;
        assert!(verify_api_origin(&plan, &execution, &files, &retained, "amd64").is_err());
        execution.start.as_mut().unwrap().inherited_decisions = 1;
        assert!(verify_api_origin(&plan, &execution, &files, &retained, "arm64").is_err());
        let mut changed = metadata;
        changed["memory"]["sha256"] = "b".repeat(64).into();
        retained.insert(
            "container/run/checkpoint/metadata.json".into(),
            serde_json::to_vec(&changed).unwrap(),
        );
        assert!(verify_api_origin(&plan, &execution, &files, &retained, "amd64").is_err());
        files.remove("container/run/checkpoint/memory");
        assert!(verify_api_origin(&plan, &execution, &files, &retained, "amd64").is_err());
        plan["format"] = "theseus-replay-plan-v2".into();
        assert!(verify_api_origin(&plan, &execution, &files, &retained, "amd64").is_err());
    }
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::path::PathBuf;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const TAG: &str = "0123456789ab";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn verifies_complete_native_evidence_pair() {
        let temporary = tempfile::tempdir().unwrap();
        for architecture in ["amd64", "arm64"] {
            write_architecture(temporary.path(), architecture, "passed");
        }
        let index = write_index(temporary.path(), true);
        assert_eq!(
            verify_native_evidence(index).unwrap(),
            NativeEvidenceSummary {
                source_commit: COMMIT.to_owned(),
                runtime_tag: TAG.to_owned(),
                architectures: vec!["amd64".to_owned(), "arm64".to_owned()],
            }
        );
    }

    #[test]
    fn verifies_one_published_architecture() {
        let temporary = tempfile::tempdir().unwrap();
        write_architecture(temporary.path(), "amd64", "passed");
        let index = write_index(temporary.path(), false);
        assert_eq!(
            verify_native_evidence(index).unwrap().architectures,
            vec!["amd64"]
        );
    }

    #[test]
    fn checkpoint_certificate_binds_origin_and_rejects_downgrades() {
        let temporary = tempfile::tempdir().unwrap();
        write_architecture(temporary.path(), "amd64", "passed");
        let path = temporary
            .path()
            .join(format!("theseus-{TAG}-runtime-certificate-amd64.json"));
        let mut certificate: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let plan =
            serde_json::json!({"format": "theseus-compose-plan-v1", "services": {"service": {}},
            "replay_start": "ready_checkpoint", "starting_checkpoint": {"sha256": DIGEST},
            "checkpoint_prefixes": {"service": 1}})
            .to_string();
        certificate["format"] = CERTIFICATE_FORMAT_V5.into();
        certificate["source"] =
            serde_json::json!({"plan_contents": plan, "plan_sha256": sha256(plan.as_bytes())});
        certificate["services"]["service"]["machine_execution_trace_decisions"] = 2.into();
        certificate["services"]["service"]["machine_execution_ledger"]["decisions"] = 2.into();
        certificate["services"]["service"]["execution_start"] = serde_json::json!({
            "kind": "topology_checkpoint", "checkpoint_sha256": DIGEST, "inherited_decisions": 1});
        let verify = |value: &serde_json::Value| {
            verify_certificate(&path, &serde_json::to_vec(value).unwrap(), "amd64")
        };
        verify(&certificate).unwrap();
        let mut wrong = certificate.clone();
        wrong["services"]["service"]["execution_start"]["checkpoint_sha256"] =
            "b".repeat(64).into();
        assert!(verify(&wrong).is_err());
        wrong = certificate.clone();
        wrong["services"]["service"]["execution_start"]["inherited_decisions"] = 2.into();
        assert!(verify(&wrong).is_err());
        wrong = certificate.clone();
        wrong["services"]["service"]
            .as_object_mut()
            .unwrap()
            .remove("execution_start");
        assert!(verify(&wrong).is_err());
        wrong = certificate.clone();
        wrong["format"] = CERTIFICATE_FORMAT_V4.into();
        assert!(verify(&wrong).is_err());
    }

    #[test]
    fn fixed_plan_archive_requires_ram_ancestry_and_active_replay() {
        let mut files = BTreeMap::new();
        let mut retained = BTreeMap::new();
        fn store(
            name: &str,
            value: &serde_json::Value,
            files: &mut BTreeMap<String, ProofFile>,
            retained: &mut BTreeMap<String, Vec<u8>>,
        ) {
            let bytes = serde_json::to_vec(value).unwrap();
            files.insert(
                name.to_owned(),
                ProofFile {
                    sha256: sha256(&bytes),
                    bytes: bytes.len() as u64,
                },
            );
            retained.insert(name.to_owned(), bytes);
        }
        let prefix = "vcpu:0:pio_read:0x64:1:00";
        let trace = [
            prefix,
            "host:serial_input:1:2a",
            "vcpu:0:pio_write:0x64:1:fe",
        ];
        let root = "fixed-plan/first/checkpoint/starting-state";
        let metadata = serde_json::json!({"format": "theseus-topology-checkpoint-v1", "architecture": "amd64",
            "context": {"bytes": 1, "sha256": DIGEST}, "execution_prefixes": {"service": [prefix]},
            "services": {"service": {"memory": {"bytes": 1024*1024, "sha256": DIGEST}, "vmstate": {"bytes": 1, "sha256": DIGEST}}}});
        let metadata_path = format!("{root}/metadata.json");
        store(&metadata_path, &metadata, &mut files, &mut retained);
        let identity = files[&metadata_path].sha256.clone();
        for (name, bytes) in [
            ("context.bin", 1),
            ("service/vmstate", 1),
            ("service/memory", 1024 * 1024),
        ] {
            files.insert(
                format!("{root}/{name}"),
                ProofFile {
                    sha256: DIGEST.to_owned(),
                    bytes,
                },
            );
        }
        let plan = serde_json::json!({"format": "theseus-compose-plan-v1", "replay_start": "ready_checkpoint",
            "starting_checkpoint": {"path": "checkpoint/starting-state/metadata.json", "sha256": identity},
            "checkpoint_prefixes": {"service": 1}, "services": {"service": {"run": {"run": {"vcpu_count": 1, "mem_size_mib": 1}}}}});
        store(
            "fixed-plan/first/replay-plan.json",
            &plan,
            &mut files,
            &mut retained,
        );
        let certificate = serde_json::json!({"source": {"plan_contents": String::from_utf8(retained["fixed-plan/first/replay-plan.json"].clone()).unwrap()}});
        store(
            "fixed-plan/certificate.json",
            &certificate,
            &mut files,
            &mut retained,
        );
        let certificate_bytes = retained["fixed-plan/certificate.json"].clone();
        let ledger = |decisions: Vec<&str>| {
            let mut digest = Sha256::new();
            for record in &decisions {
                digest.update((record.len() as u64).to_le_bytes());
                digest.update(record.as_bytes());
            }
            serde_json::json!({"decisions": decisions.len(), "sha256": format!("{:x}", digest.finalize()), "tail": decisions})
        };
        let result = serde_json::json!({"status": "passed", "error": null, "serial_sha256": [DIGEST],
            "execution_start": {"kind": "topology_checkpoint", "checkpoint_sha256": identity, "inherited_decisions": 1},
            "machine_execution_trace": trace, "machine_execution_ledger": ledger(trace.to_vec()),
            "execution_ledgers": [ledger(vec!["pio_read:0x64:1:00", "pio_write:0x64:1:fe"])],
            "checks": [{"name": "replay_machine_execution_trace", "status": "passed"}]});
        for execution in ["first", "replay"] {
            store(
                &format!("fixed-plan/{execution}/services/service/result.json"),
                &result,
                &mut files,
                &mut retained,
            );
            files.insert(
                format!("fixed-plan/{execution}/services/service/serial.log"),
                ProofFile {
                    sha256: DIGEST.to_owned(),
                    bytes: 1,
                },
            );
        }
        verify_fixed_plan(&files, &retained, "amd64", &certificate_bytes).unwrap();
        assert!(verify_fixed_plan(&files, &retained, "arm64", &certificate_bytes).is_err());
        let memory = files.remove(&format!("{root}/service/memory")).unwrap();
        assert!(verify_fixed_plan(&files, &retained, "amd64", &certificate_bytes).is_err());
        files.insert(format!("{root}/service/memory"), memory);
        let mut wrong = result.clone();
        wrong["checks"] = serde_json::json!([]);
        store(
            "fixed-plan/replay/services/service/result.json",
            &wrong,
            &mut files,
            &mut retained,
        );
        assert!(verify_fixed_plan(&files, &retained, "amd64", &certificate_bytes).is_err());
        wrong = result.clone();
        wrong["execution_start"]["inherited_decisions"] = 2.into();
        store(
            "fixed-plan/replay/services/service/result.json",
            &wrong,
            &mut files,
            &mut retained,
        );
        assert!(verify_fixed_plan(&files, &retained, "amd64", &certificate_bytes).is_err());
    }

    #[test]
    fn verifies_legacy_certificate_and_validation_formats() {
        let temporary = tempfile::tempdir().unwrap();
        write_architecture_with_formats(temporary.path(), "amd64", "passed", true);
        let index = write_index(temporary.path(), false);
        assert_eq!(
            verify_native_evidence(index).unwrap().architectures,
            vec!["amd64"]
        );
    }

    #[test]
    fn rejects_a_failed_replay_inside_a_consistent_archive() {
        let temporary = tempfile::tempdir().unwrap();
        write_architecture(temporary.path(), "amd64", "failed");
        write_architecture(temporary.path(), "arm64", "passed");
        let index = write_index(temporary.path(), true);
        let error = verify_native_evidence(index).unwrap_err();
        assert!(error
            .to_string()
            .contains("one or more replay services did not pass"));
    }

    #[test]
    fn verifies_a_host_input_container_replay_with_different_guest_execution() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();
        write_architecture(directory, "amd64", "passed");
        let root = directory.join("validation-amd64/validation");

        let plan_path = root.join("container/run/replay-plan.json");
        let mut plan: serde_json::Value =
            serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
        plan["run"]["machine_replay"] = serde_json::json!("host_inputs");
        fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();

        let replay_path = root.join("container/rerun/execution.json");
        let mut replay: serde_json::Value =
            serde_json::from_slice(&fs::read(&replay_path).unwrap()).unwrap();
        let record = "vcpu:0:pio_write:0x64:1:20";
        let local = "pio_write:0x64:1:20";
        let ledger = |value: &str| {
            let mut digest = Sha256::new();
            digest.update((value.len() as u64).to_le_bytes());
            digest.update(value.as_bytes());
            serde_json::json!({
                "decisions": 1,
                "sha256": format!("{:x}", digest.finalize()),
                "tail": [value]
            })
        };
        replay["machine_execution_trace"] = serde_json::json!([record]);
        replay["machine_execution_ledger"] = ledger(record);
        replay["execution_ledgers"] = serde_json::json!([ledger(local)]);
        fs::write(&replay_path, serde_json::to_vec(&replay).unwrap()).unwrap();

        let mut proof: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("evidence.json")).unwrap()).unwrap();
        let mut inventory = BTreeMap::new();
        inventory_tree(&root, &root, &mut inventory);
        inventory.remove("evidence.json");
        proof["files"] = serde_json::to_value(inventory).unwrap();
        fs::write(
            root.join("evidence.json"),
            serde_json::to_vec(&proof).unwrap(),
        )
        .unwrap();
        archive_validation(directory, "amd64", &root);

        let index = write_index(directory, false);
        assert_eq!(
            verify_native_evidence(index).unwrap().architectures,
            vec!["amd64"]
        );
    }

    #[test]
    fn rejects_semantically_empty_runtime_validation() {
        let temporary = tempfile::tempdir().unwrap();
        write_architecture(temporary.path(), "amd64", "passed");
        write_validation(temporary.path(), "amd64", 0, false);
        let index = write_index(temporary.path(), false);
        let error = verify_native_evidence(index).unwrap_err();
        assert!(error
            .to_string()
            .contains("coverage retained no unique_application_blocks"));
    }

    #[test]
    fn rejects_partial_execution_evidence_in_any_retained_run() {
        let partial = serde_json::to_vec(&serde_json::json!({
            "runs": [
                {"execution_ledgers": {"api": [{
                    "decisions": 1,
                    "sha256": DIGEST,
                    "tail": ["mmio_read:0x0:1:00"]
                }]}},
                {"execution_ledgers": {}}
            ]
        }))
        .unwrap();
        assert!(
            verify_campaign_execution_ledgers(&partial, "strict execution")
                .unwrap_err()
                .to_string()
                .contains("no valid ordered KVM execution ledger")
        );
    }

    #[test]
    fn v5_rejects_changed_runtime_replay_evidence_even_after_resealing() {
        for mutation in ["missing", "digest", "pause", "check", "schedule"] {
            let temporary = tempfile::tempdir().unwrap();
            let directory = temporary.path();
            write_architecture(directory, "amd64", "passed");
            let root = directory.join("validation-amd64/validation");
            if mutation == "missing" {
                fs::remove_file(root.join("container/rerun/execution.json")).unwrap();
            } else {
                let name = match mutation {
                    "check" => "container/rerun/result.json",
                    "schedule" => "schedule-search/rerun/services/ledger/result.json",
                    _ => "container/rerun/execution.json",
                };
                let path = root.join(name);
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                match mutation {
                    "digest" => {
                        value["machine_execution_ledger"]["sha256"] =
                            serde_json::json!("0".repeat(64))
                    }
                    "pause" => value["boundary"] = serde_json::json!("pause"),
                    "check" => value["checks"][0]["status"] = serde_json::json!("failed"),
                    "schedule" => value["checks"][1]["status"] = serde_json::json!("failed"),
                    _ => unreachable!(),
                }
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            let mut proof: serde_json::Value =
                serde_json::from_slice(&fs::read(root.join("evidence.json")).unwrap()).unwrap();
            let mut inventory = BTreeMap::new();
            inventory_tree(&root, &root, &mut inventory);
            inventory.remove("evidence.json");
            proof["files"] = serde_json::to_value(inventory).unwrap();
            fs::write(
                root.join("evidence.json"),
                serde_json::to_vec(&proof).unwrap(),
            )
            .unwrap();
            archive_validation(directory, "amd64", &root);
            let index = write_index(directory, false);
            assert!(
                verify_native_evidence(index).is_err(),
                "mutation {mutation} accepted"
            );
        }
    }

    #[test]
    fn rejects_machine_traces_with_missing_services_or_malformed_actors() {
        let result = |traces: serde_json::Value| {
            let api_decisions = traces["api"].as_array().map_or(1, Vec::len);
            let worker_decisions = traces["worker"].as_array().map_or(1, Vec::len);
            serde_json::to_vec(&serde_json::json!({
                "runs": [{
                    "machine_execution_ledgers": {
                        "api": {"decisions": api_decisions},
                        "worker": {"decisions": worker_decisions}
                    },
                    "machine_execution_traces": traces
                }]
            }))
            .unwrap()
        };
        for traces in [
            serde_json::json!({"api": ["vcpu:0:mmio_read:0x0:1:00"]}),
            serde_json::json!({
                "api": ["vcpu:not-a-number:mmio_read:0x0:1:00"],
                "worker": ["vcpu:0:mmio_read:0x0:1:00"]
            }),
            serde_json::json!({
                "api": ["host:serial_input:2:2a"],
                "worker": ["host:virtual_time_jump:1000"]
            }),
            serde_json::json!({
                "api": ["host:control_event:AF"],
                "worker": ["host:virtual_time_jump:1000"]
            }),
            serde_json::json!({
                "api": ["host:unknown:payload"],
                "worker": ["host:virtual_time_jump:1000"]
            }),
            serde_json::json!({
                "api": ["vcpu:0:interrupt:unknown:4"],
                "worker": ["host:virtual_time_jump:1000"]
            }),
        ] {
            assert!(
                verify_campaign_machine_execution_traces(&result(traces), "strict execution")
                    .is_err()
            );
        }

        verify_campaign_machine_execution_traces(
            &result(serde_json::json!({
                "api": [
                    "host:serial_input:2:2a0a",
                    "host:ctrl_alt_del",
                    "vcpu:0:interrupt:virtio-mmio:5"
                ],
                "worker": ["host:virtual_time_jump:1000"]
            })),
            "strict execution",
        )
        .unwrap();
    }

    fn write_architecture(directory: &Path, architecture: &str, replay_status: &str) {
        write_architecture_with_formats(directory, architecture, replay_status, false);
    }

    fn write_architecture_with_formats(
        directory: &Path,
        architecture: &str,
        replay_status: &str,
        legacy: bool,
    ) {
        let certificate_name = format!("theseus-{TAG}-runtime-certificate-{architecture}.json");
        let plan_contents = r#"{"format":"theseus-compose-plan-v1","services":{"service":{}}}"#;
        let certificate = serde_json::to_vec_pretty(&serde_json::json!({
            "format": if legacy { CERTIFICATE_FORMAT_V1 } else { CERTIFICATE_FORMAT_V4 },
            "status": "passed",
            "profile": {"id": "linux-kvm-simulated-io-v1", "architecture": architecture},
            "source": {
                "plan_sha256": sha256(plan_contents.as_bytes()),
                "plan_contents": plan_contents
            },
            "repeatability": {"executions": 2},
            "services": {"service": {
                "execution_ledgers": [{
                    "decisions": 1,
                    "sha256": DIGEST,
                    "tail": ["mmio_read addr=0x0 len=1"]
                }],
                "machine_execution_ledger": {
                    "decisions": 1,
                    "sha256": DIGEST,
                    "tail": ["vcpu:0:mmio_read addr=0x0 len=1"]
                },
                "machine_execution_trace_decisions": 1
            }}
        }))
        .unwrap();
        fs::write(directory.join(&certificate_name), &certificate).unwrap();

        let root = directory.join(format!("root-{architecture}/minimized"));
        let evidence = root.join("evidence");
        let replay = evidence.join("replay/services");
        for service in ["counter", "writer-a", "writer-b"] {
            fs::create_dir_all(replay.join(service)).unwrap();
            fs::write(
                replay.join(service).join("result.json"),
                if service == "writer-a" {
                    format!(
                        r#"{{"status":"{replay_status}","network_traffic":{{"backplane":{{"dropped":1}}}}}}"#
                    )
                } else {
                    format!(r#"{{"status":"{replay_status}","network_traffic":{{}}}}"#)
                },
            )
            .unwrap();
        }
        fs::write(evidence.join("runtime-certificate.json"), &certificate).unwrap();
        fs::write(
            evidence.join("campaign-result.json"),
            serde_json::to_vec(&serde_json::json!({
                "format": "theseus-compose-campaign-result-v1",
                "properties": [
                    {"name": "network_recovery_is_reachable", "status": "passed"},
                    {"name": "sequential_result_is_reachable", "status": "passed"},
                    {"name": PROPERTY, "status": "failed"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            evidence.join("replay/topology-result.json"),
            br#"{"actions":[{"kind":"partition"},{"kind":"heal"}]}"#,
        )
        .unwrap();
        fs::write(replay.join("counter/serial.log"), b"{\"value\":1}\n").unwrap();
        fs::write(
            replay.join("writer-a/serial.log"),
            b"{\"network\":\"recovered\"}\n",
        )
        .unwrap();
        fs::write(
            root.join("minimization.json"),
            serde_json::to_vec(&serde_json::json!({
                "property": PROPERTY,
                "minimized_faults": REQUIRED_FAULTS
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(root.join("replay-plan.json"), b"{}").unwrap();
        let mut inventory = BTreeMap::new();
        inventory_tree(&root, &root, &mut inventory);
        let proof = serde_json::to_vec_pretty(&serde_json::json!({
            "format": PROOF_FORMAT,
            "architecture": architecture,
            "source_commit": COMMIT,
            "runtime": {
                "image": format!("ghcr.io/e6qu/theseus@sha256:{DIGEST}"),
                "tag": format!("{TAG}-{architecture}")
            },
            "host": {"kernel_release": "6.8.0", "kvm_api_version": 12},
            "property": PROPERTY,
            "required_faults": REQUIRED_FAULTS,
            "files": inventory,
        }))
        .unwrap();
        fs::write(evidence.join("proof.json"), proof).unwrap();

        let archive_name =
            format!("theseus-{TAG}-multiservice-counterexample-{architecture}.tar.gz");
        let archive = fs::File::create(directory.join(archive_name)).unwrap();
        let encoder = GzEncoder::new(archive, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder.append_dir_all("minimized", &root).unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        write_validation(directory, architecture, 1, legacy);
    }

    fn write_validation(directory: &Path, architecture: &str, signal_count: u64, legacy: bool) {
        let root = directory.join(format!("validation-{architecture}/validation"));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        let plan = br#"{"format":"theseus-compose-plan-v1"}"#;
        let run_plan = br#"{"format":"theseus-run-plan-v1"}"#;
        let campaign = |signal: &str, replay: bool| {
            let mut value = serde_json::json!({
                "format": "theseus-compose-campaign-result-v1",
                (signal): signal_count
            });
            if replay {
                value["replay_verification"] = serde_json::json!({"status": "passed"});
            }
            if signal == "thread_scheduling_decisions" {
                value["properties"] = serde_json::json!([{
                    "name": "lost_update_is_unreachable",
                    "status": "failed"
                }]);
            }
            if signal == "execution_decisions" {
                value["runs"] = serde_json::json!([{
                    "execution_ledgers": {"api": [{
                        "decisions": signal_count,
                        "sha256": DIGEST,
                        "tail": ["mmio_read addr=0x0 len=1"]
                    }]},
                    "machine_execution_ledgers": {"api": {
                        "decisions": signal_count,
                        "sha256": DIGEST,
                        "tail": ["vcpu:0:mmio_read addr=0x0 len=1"]
                    }},
                    "machine_execution_traces": {"api": [
                        "vcpu:0:mmio_read:0x0:1:00"
                    ]}
                }]);
            }
            serde_json::to_vec(&value).unwrap()
        };
        let ledger = |record: &str| {
            let mut digest = Sha256::new();
            digest.update((record.len() as u64).to_le_bytes());
            digest.update(record.as_bytes());
            serde_json::json!({
                "decisions": 1,
                "sha256": format!("{:x}", digest.finalize()),
                "tail": [record]
            })
        };
        let schedule_replay_plan = serde_json::to_vec(&serde_json::json!({
            "format": "theseus-compose-plan-v1",
            "machine_replay": "host_inputs",
            "campaign": null,
            "services": {"ledger": {"run": {"run": {"vcpu_count": 1}}}}
        }))
        .unwrap();
        let schedule_replay_result = serde_json::to_vec(&serde_json::json!({
            "status": "passed",
            "error": null,
            "checks": [
                {"name": "campaign_checkpoint", "status": "passed"},
                {"name": "counterexample: lost_update_is_unreachable", "status": "passed"},
                {"name": "replay_machine_execution_trace", "status": "passed"}
            ],
            "execution_ledgers": [ledger("pio_write:0x64:1:fe")],
            "machine_execution_ledger": ledger("vcpu:0:pio_write:0x64:1:fe"),
            "machine_execution_trace": ["vcpu:0:pio_write:0x64:1:fe"]
        }))
        .unwrap();
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("container/plan.json", run_plan.to_vec()),
            ("container/run/replay-plan.json", b"{}".to_vec()),
            (
                "container/run/result.json",
                br#"{"status":"passed"}"#.to_vec(),
            ),
            (
                "container/run/serial.log",
                b"THES:HTTP:operation:read_health:PASS\n".to_vec(),
            ),
            ("container/replay.log", b"replay passed\n".to_vec()),
            ("container/source/Dockerfile", b"FROM scratch\n".to_vec()),
            ("container/source/theseus.toml", b"version = 1\n".to_vec()),
            ("coverage/plan.json", plan.to_vec()),
            (
                "coverage/campaign/campaign-result.json",
                campaign("unique_application_blocks", false),
            ),
            ("coverage/campaign/replay-plan.json", b"{}".to_vec()),
            ("coverage/report/report.md", b"coverage\n".to_vec()),
            (
                "coverage/rerun/campaign-result.json",
                campaign("unique_application_blocks", true),
            ),
            (
                "coverage/comparison.json",
                br#"{"format":"theseus-campaign-comparison-v1","status":"same"}"#.to_vec(),
            ),
            (
                "coverage/evaluation.json",
                br#"{"format":"theseus-public-evaluation-v1","status":"passed","workloads":[{"name":"coverage"}]}"#.to_vec(),
            ),
            (
                "coverage/evaluation/theseus-evaluation.toml",
                b"format = 2\n".to_vec(),
            ),
            (
                "coverage/evaluation/theseus-evaluation.lock",
                b"{}".to_vec(),
            ),
            ("coverage/source/compose.yaml", b"services: {}\n".to_vec()),
            (
                "coverage/source/service/main.c",
                b"int main(void) {}\n".to_vec(),
            ),
            (
                "coverage/source/service/theseus.toml",
                b"version = 1\n".to_vec(),
            ),
            ("schedule-search/plan.json", plan.to_vec()),
            (
                "schedule-search/campaign/campaign-result.json",
                campaign("thread_scheduling_decisions", false),
            ),
            ("schedule-search/campaign/replay-plan.json", b"{}".to_vec()),
            ("schedule-search/report/report.md", b"schedule\n".to_vec()),
            (
                "schedule-search/minimized/minimization.json",
                br#"{"property":"lost_update_is_unreachable"}"#.to_vec(),
            ),
            (
                "schedule-search/minimized/replay-plan.json",
                schedule_replay_plan.clone(),
            ),
            (
                "schedule-search/minimized/services/ledger/result.json",
                schedule_replay_result.clone(),
            ),
            (
                "schedule-search/minimized/topology-result.json",
                b"{}".to_vec(),
            ),
            (
                "schedule-search/rerun/replay-plan.json",
                schedule_replay_plan,
            ),
            (
                "schedule-search/rerun/services/ledger/result.json",
                schedule_replay_result,
            ),
            (
                "schedule-search/rerun/topology-result.json",
                b"{}".to_vec(),
            ),
            (
                "schedule-search/source/compose.yaml",
                b"services: {}\n".to_vec(),
            ),
            (
                "schedule-search/source/service/main.c",
                b"int main(void) {}\n".to_vec(),
            ),
            (
                "schedule-search/source/service/theseus.toml",
                b"version = 1\n".to_vec(),
            ),
            ("pthread-sync/plan.json", plan.to_vec()),
            (
                "pthread-sync/campaign/campaign-result.json",
                campaign("thread_synchronization_events", false),
            ),
            ("pthread-sync/campaign/replay-plan.json", b"{}".to_vec()),
            (
                "pthread-sync/report/report.md",
                b"synchronization\n".to_vec(),
            ),
            (
                "pthread-sync/rerun/campaign-result.json",
                campaign("thread_synchronization_events", true),
            ),
            (
                "pthread-sync/source/compose.yaml",
                b"services: {}\n".to_vec(),
            ),
            (
                "pthread-sync/source/service/main.c",
                b"int main(void) {}\n".to_vec(),
            ),
            (
                "pthread-sync/source/service/theseus.toml",
                b"version = 1\n".to_vec(),
            ),
            ("strict-execution/plan.json", plan.to_vec()),
            (
                "strict-execution/campaign/campaign-result.json",
                campaign("execution_decisions", false),
            ),
            ("strict-execution/campaign/replay-plan.json", b"{}".to_vec()),
            (
                "strict-execution/report/report.md",
                b"# Execution ledger\n".to_vec(),
            ),
            (
                "strict-execution/rerun/campaign-result.json",
                campaign("execution_decisions", true),
            ),
            (
                "strict-execution/comparison.json",
                br#"{"format":"theseus-campaign-comparison-v1","status":"same"}"#.to_vec(),
            ),
            (
                "strict-execution/source/.dockerignore",
                b"api/work\n".to_vec(),
            ),
            (
                "strict-execution/source/Dockerfile",
                b"FROM scratch\n".to_vec(),
            ),
            (
                "strict-execution/source/compose.yaml",
                b"services: {}\n".to_vec(),
            ),
            (
                "strict-execution/source/api/theseus.toml",
                b"version = 1\n".to_vec(),
            ),
        ];
        for (name, bytes) in files {
            if legacy && name.starts_with("strict-execution/") {
                continue;
            }
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        if !legacy {
            let execution = serde_json::json!({
                "format": "theseus-execution-v1", "boundary": "guest_exit", "replay_error": null,
                "execution_ledgers": [ledger("pio_write:0x64:1:fe")],
                "machine_execution_ledger": ledger("vcpu:0:pio_write:0x64:1:fe"),
                "machine_execution_trace": ["vcpu:0:pio_write:0x64:1:fe"],
            });
            fs::create_dir_all(root.join("container/rerun")).unwrap();
            for name in [
                "container/run/execution.json",
                "container/rerun/execution.json",
            ] {
                fs::write(root.join(name), serde_json::to_vec(&execution).unwrap()).unwrap();
            }
            fs::write(
                root.join("container/run/replay-plan.json"),
                br#"{"format":"theseus-replay-plan-v2","run":{"vcpu_count":1}}"#,
            )
            .unwrap();
            fs::write(
                root.join("container/run/result.json"),
                br#"{"status":"passed","execution_evidence":"execution.json"}"#,
            )
            .unwrap();
            fs::write(root.join("container/rerun/result.json"), br#"{"status":"passed","execution_evidence":"execution.json","checks":[{"name":"replay_machine_execution","status":"passed"}]}"#).unwrap();
            fs::write(
                root.join("container/rerun/serial.log"),
                b"THES:HTTP:operation:read_health:PASS\n",
            )
            .unwrap();
        }
        let mut inventory = BTreeMap::new();
        inventory_tree(&root, &root, &mut inventory);
        fs::write(
            root.join("evidence.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "format": if legacy { VALIDATION_FORMAT_V1 } else { VALIDATION_FORMAT_V5 },
                "architecture": architecture,
                "source_commit": COMMIT,
                "runtime": {
                    "image": format!("ghcr.io/e6qu/theseus@sha256:{DIGEST}"),
                    "tag": format!("{TAG}-{architecture}")
                },
                "host": {"kernel_release": "6.8.0", "kvm_api_version": 12},
                "scenarios": if legacy {
                    vec!["container", "coverage", "schedule-search", "pthread-sync"]
                } else {
                    vec!["container", "coverage", "schedule-search", "pthread-sync", "strict-execution"]
                },
                "files": inventory,
            }))
            .unwrap(),
        )
        .unwrap();
        archive_validation(directory, architecture, &root);
    }

    fn archive_validation(directory: &Path, architecture: &str, root: &Path) {
        let archive_name = format!("theseus-{TAG}-runtime-validation-{architecture}.tar.gz");
        let archive = fs::File::create(directory.join(archive_name)).unwrap();
        let encoder = GzEncoder::new(archive, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder.append_dir_all("validation", &root).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }

    fn inventory_tree(
        root: &Path,
        directory: &Path,
        inventory: &mut BTreeMap<String, serde_json::Value>,
    ) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                inventory_tree(root, &path, inventory);
            } else {
                let bytes = fs::read(&path).unwrap();
                inventory.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    serde_json::json!({"sha256": sha256(&bytes), "bytes": bytes.len()}),
                );
            }
        }
    }

    fn write_index(directory: &Path, complete: bool) -> PathBuf {
        let architectures = if complete {
            vec!["amd64", "arm64"]
        } else {
            vec!["amd64"]
        };
        let architectures = architectures
            .into_iter()
            .map(|architecture| {
                let certificate = format!("theseus-{TAG}-runtime-certificate-{architecture}.json");
                let counterexample =
                    format!("theseus-{TAG}-multiservice-counterexample-{architecture}.tar.gz");
                let validation = format!("theseus-{TAG}-runtime-validation-{architecture}.tar.gz");
                let (certificate_sha256, certificate_bytes) =
                    asset_metadata(&directory.join(&certificate));
                let (counterexample_sha256, counterexample_bytes) =
                    asset_metadata(&directory.join(&counterexample));
                let (validation_sha256, validation_bytes) =
                    asset_metadata(&directory.join(&validation));
                (
                    architecture.to_owned(),
                    serde_json::json!({
                        "certificate": {
                            "file": certificate,
                            "sha256": certificate_sha256,
                            "bytes": certificate_bytes
                        },
                        "counterexample": {
                            "file": counterexample,
                            "sha256": counterexample_sha256,
                            "bytes": counterexample_bytes
                        },
                        "validation": {
                            "file": validation,
                            "sha256": validation_sha256,
                            "bytes": validation_bytes
                        }
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let path = directory.join(format!("theseus-{TAG}-native-evidence.json"));
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "format": INDEX_FORMAT,
                "source_commit": COMMIT,
                "runtime_tag": TAG,
                "architectures": architectures
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    fn asset_metadata(path: &Path) -> (String, usize) {
        let bytes = fs::read(path).unwrap();
        (sha256(&bytes), bytes.len())
    }
}
