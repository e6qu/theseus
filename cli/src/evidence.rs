// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Offline verification for the complete, dual-architecture native KVM
//! evidence set attached to a SHA release.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path};

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::Archive;

const INDEX_FORMAT: &str = "theseus-native-evidence-index-v1";
const PROOF_FORMAT: &str = "theseus-counterexample-proof-v2";
const CERTIFICATE_FORMAT: &str = "theseus-runtime-certificate-v1";
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

/// Verify the signed index's files, both certificates, each archive inventory,
/// and the required recovery-before-counterexample observations without KVM.
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
    let expected = BTreeSet::from(["amd64".to_owned(), "arm64".to_owned()]);
    require(
        index.architectures.keys().cloned().collect::<BTreeSet<_>>() == expected,
        "native evidence index must contain exactly amd64 and arm64",
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
        require(
            evidence.certificate.file == certificate_name,
            &format!("unexpected {architecture} certificate filename"),
        )?;
        require(
            evidence.counterexample.file == counterexample_name,
            &format!("unexpected {architecture} counterexample filename"),
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
        certificate.format == CERTIFICATE_FORMAT,
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
    )
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
        if required.contains(name.as_str()) {
            entry.read_to_end(&mut bytes).map_err(|error| {
                EvidenceError(format!("cannot read archive member {name}: {error}"))
            })?;
            hasher.update(&bytes);
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

fn require_safe_archive_path(path: &Path) -> Result<(), EvidenceError> {
    require(
        !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "counterexample archive contains an unsafe path",
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
    fn rejects_an_incomplete_architecture_set() {
        let temporary = tempfile::tempdir().unwrap();
        write_architecture(temporary.path(), "amd64", "passed");
        let index = write_index(temporary.path(), false);
        let error = verify_native_evidence(index).unwrap_err();
        assert!(error.to_string().contains("exactly amd64 and arm64"));
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

    fn write_architecture(directory: &Path, architecture: &str, replay_status: &str) {
        let certificate_name = format!("theseus-{TAG}-runtime-certificate-{architecture}.json");
        let plan_contents = r#"{"format":"theseus-compose-plan-v1","services":{"service":{}}}"#;
        let certificate = serde_json::to_vec_pretty(&serde_json::json!({
            "format": CERTIFICATE_FORMAT,
            "status": "passed",
            "profile": {"id": "linux-kvm-simulated-io-v1", "architecture": architecture},
            "source": {
                "plan_sha256": sha256(plan_contents.as_bytes()),
                "plan_contents": plan_contents
            },
            "repeatability": {"executions": 2}
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
                let (certificate_sha256, certificate_bytes) =
                    asset_metadata(&directory.join(&certificate));
                let (counterexample_sha256, counterexample_bytes) =
                    asset_metadata(&directory.join(&counterexample));
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
