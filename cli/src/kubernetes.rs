// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kubernetes manifest input for the documented supported subset.
//!
//! Theseus services are single-container VMs with one immutable image, one
//! entrypoint contract, and named networks; this module translates the
//! Kubernetes subset that maps onto that model - Pods, Deployments,
//! StatefulSets, DaemonSets, ReplicaSets, and Jobs with exactly one
//! container, and ClusterIP Services as network membership -
//! into the same `ComposeFile` the Compose path produces, so the locked
//! plan, campaign, and evidence pipeline are identical. Everything outside
//! the subset is rejected with a naming error, never silently dropped:
//! multiple containers, init containers, volumes, ConfigMaps and Secrets,
//! `valueFrom` environment references, non-ClusterIP Services, host
//! networking, and ports.
//!
//! Every translated service needs a Theseus service manifest (runtime,
//! kernel, guest image) exactly as Compose requires. The default is
//! `theseus.toml` beside the Kubernetes manifest; a Pod overrides it per
//! service with the annotation `theseus.io/manifest: path`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::compose::{
    ComposeConfigMount, ComposeEnvironment, ComposeFile, ComposeNetwork, ComposeService,
    ComposeServiceConfig, ComposeTheseus, ServiceTheseus,
};
use crate::ComposeError;

/// The annotation that overrides the default service manifest per service.
pub const MANIFEST_ANNOTATION: &str = "theseus.io/manifest";

#[derive(Debug, Deserialize)]
struct ManifestDocument {
    #[serde(rename = "apiVersion", default)]
    api_version: Option<String>,
    #[serde(rename = "kind")]
    kind: Option<String>,
    #[serde(default)]
    metadata: Option<Metadata>,
    #[serde(default)]
    spec: serde_json::Value,
    /// ConfigMap plain entries (top-level on ConfigMap documents).
    #[serde(default)]
    data: Option<serde_json::Value>,
    /// ConfigMap base64 entries.
    #[serde(rename = "binaryData", default)]
    binary_data: Option<serde_json::Value>,
    /// Secret plain entries.
    #[serde(rename = "stringData", default)]
    string_data: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

/// True when a file looks like a Kubernetes manifest rather than a Compose
/// file: Compose has no `apiVersion` at the document root.
pub fn looks_like_kubernetes(input: &str) -> bool {
    #[derive(Deserialize)]
    struct Sniff {
        #[serde(rename = "apiVersion")]
        api_version: Option<serde_yaml::Value>,
    }
    // Multi-document manifests are common; only the first document needs to
    // carry the Kubernetes apiVersion discriminant.
    serde_yaml::Deserializer::from_str(input)
        .next()
        .map(|document| {
            Sniff::deserialize(document)
                .map(|sniff| sniff.api_version.is_some())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn named(metadata: &Option<Metadata>, what: &str) -> Result<String, ComposeError> {
    metadata
        .as_ref()
        .and_then(|metadata| metadata.name.clone())
        .ok_or_else(|| ComposeError::Invalid(format!("Kubernetes {what} has no metadata.name")))
}

fn pod_containers<'a>(
    service_name: &str,
    spec: &'a serde_json::Value,
) -> Result<Vec<&'a serde_json::Value>, ComposeError> {
    let reject = |field: &str| {
        ComposeError::Invalid(format!(
            "Kubernetes service {service_name:?} is outside the supported subset: {field} is not supported"
        ))
    };
    for unsupported in [
        "initContainers",
        "hostNetwork",
        "hostPID",
        "hostIPC",
        "nodeName",
        "affinity",
        "tolerations",
    ] {
        if spec.get(unsupported).is_some_and(|value| !value.is_null()) {
            return Err(reject(unsupported));
        }
    }
    let containers = spec
        .get("containers")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            ComposeError::Invalid(format!(
                "Kubernetes service {service_name:?} has no containers"
            ))
        })?;
    if containers.is_empty() {
        return Err(ComposeError::Invalid(format!(
            "Kubernetes service {service_name:?} has no containers"
        )));
    }
    for container in containers {
        for unsupported in ["ports", "livenessProbe", "readinessProbe"] {
            if container
                .get(unsupported)
                .is_some_and(|value| !value.is_null())
            {
                return Err(reject(unsupported));
            }
        }
        if container
            .get("env")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry.get("valueFrom").is_some_and(|v| !v.is_null()))
            })
        {
            return Err(reject("env valueFrom references"));
        }
    }
    Ok(containers.iter().collect())
}

/// One translated container's service name: the pod's name for a
/// single-container pod, `pod-container` for each container of a
/// multi-container pod.
fn translated_service_name(
    pod: &str,
    container: &serde_json::Value,
    count: usize,
) -> Result<String, ComposeError> {
    if count == 1 {
        return Ok(pod.to_owned());
    }
    let name = container
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ComposeError::Invalid(format!(
                "Kubernetes pod {pod:?} declares multiple containers; every container needs a name"
            ))
        })?;
    Ok(format!("{pod}-{name}"))
}

/// A container's manifest annotation: the container's own
/// `theseus.io/manifest` wins, then the pod's, then the loader default.
fn container_manifest(container: &serde_json::Value, pod_manifest: &str) -> String {
    container
        .get("metadata")
        .and_then(|metadata| metadata.get("annotations"))
        .and_then(|annotations| annotations.get(MANIFEST_ANNOTATION))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| pod_manifest.to_owned())
}

fn environment(container: &serde_json::Value) -> Option<ComposeEnvironment> {
    let entries = container.get("env").and_then(serde_json::Value::as_array)?;
    let mut map = BTreeMap::new();
    for entry in entries {
        if let (Some(name), Some(value)) = (
            entry.get("name").and_then(serde_json::Value::as_str),
            entry.get("value").and_then(serde_json::Value::as_str),
        ) {
            map.insert(name.to_owned(), value.to_owned());
        }
    }
    (!map.is_empty()).then_some(ComposeEnvironment::Map(map))
}

/// Translate one Kubernetes manifest into the Compose input model.
///
/// One parsed ConfigMap or Secret: the key-to-content entries a volume
/// mounts as files.
type InlineEntries = BTreeMap<String, Vec<u8>>;

/// The translated inputs: the Compose model plus the inline config and
/// secret bytes the Kubernetes documents carried, keyed by synthetic
/// definition name.
pub struct KubernetesInputs {
    pub compose: ComposeFile,
    pub configs: BTreeMap<String, Vec<u8>>,
    pub secrets: BTreeMap<String, Vec<u8>>,
}

/// A deterministic definition name for one mounted key. Definition names
/// allow letters, digits, '-', and '_', so everything else folds to '_'.
fn definition_name(kind: &str, source: &str, key: &str) -> String {
    let sanitize = |value: &str| {
        value
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("k8s-{kind}-{}-{}", sanitize(source), sanitize(key))
}

/// One pod volume resolved to a ConfigMap or Secret source, with the
/// optional `items` projection (key -> relative path under the mount).
#[derive(Debug, Clone)]
struct PodVolume {
    kind: String,
    source: String,
    items: Option<Vec<(String, String)>>,
}

type VolumeSources = BTreeMap<String, PodVolume>;

/// Parse a volume's optional `items` projection: an array of
/// `{key, path}` pairs remapping entry keys to relative paths under the
/// mount point.
fn parse_volume_items(
    items: Option<&serde_json::Value>,
) -> Result<Option<Vec<(String, String)>>, ComposeError> {
    let Some(items) = items.and_then(serde_json::Value::as_array) else {
        return Ok(None);
    };
    let mut projections = Vec::new();
    for item in items {
        let key = item
            .get("key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ComposeError::Invalid("Kubernetes volume items must name a key".to_owned())
            })?;
        let path = item
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ComposeError::Invalid(format!("Kubernetes volume item {key:?} must name a path"))
            })?;
        if path.starts_with('/') || path.contains("..") {
            return Err(ComposeError::Invalid(format!(
                "Kubernetes volume item path {path:?} must be relative without '..'"
            )));
        }
        projections.push((key.to_owned(), path.to_owned()));
    }
    if projections.is_empty() {
        return Ok(None);
    }
    Ok(Some(projections))
}

/// Decode a Secret's `data` entry (standard base64).
fn decode_secret_data(encoded: &str) -> Result<Vec<u8>, ComposeError> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buffer = 0_u32;
    let mut bits = 0_u32;
    for character in encoded.bytes() {
        if matches!(character, b'=' | b'\n' | b'\r' | b' ') {
            continue;
        }
        let value = ALPHABET
            .iter()
            .position(|candidate| *candidate == character)
            .ok_or_else(|| {
                ComposeError::Invalid("Kubernetes Secret data is not valid base64".to_owned())
            })? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

/// `service_manifest` is the default Theseus service manifest applied to
/// every translated service, relative to the Kubernetes manifest's
/// directory, overridable per service with the
/// `theseus.io/manifest` annotation.
pub fn load_kubernetes_compose(
    path: impl AsRef<Path>,
    service_manifest: &str,
) -> Result<KubernetesInputs, ComposeError> {
    let path = path.as_ref();
    let input = std::fs::read_to_string(path).map_err(|source| ComposeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    load_kubernetes_compose_str(&input, path, service_manifest)
}

/// Parse Kubernetes manifest text into the campaign input model. The
/// display path appears only in errors.
pub fn load_kubernetes_compose_str(
    input: &str,
    display_path: impl AsRef<Path>,
    service_manifest: &str,
) -> Result<KubernetesInputs, ComposeError> {
    let path = display_path.as_ref();
    let mut services = BTreeMap::new();
    let mut selectors: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut labels: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut config_entries: BTreeMap<String, InlineEntries> = BTreeMap::new();
    let mut secret_entries: BTreeMap<String, InlineEntries> = BTreeMap::new();
    let mut configs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut secrets: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut pod_mounts: BTreeMap<String, (Vec<serde_json::Value>, VolumeSources)> = BTreeMap::new();

    for document in serde_yaml::Deserializer::from_str(input) {
        let document =
            ManifestDocument::deserialize(document).map_err(|source| ComposeError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        let api_version = document.api_version.as_deref().ok_or_else(|| {
            ComposeError::Invalid("Kubernetes document has no apiVersion".to_owned())
        })?;
        if !api_version.contains('/') && api_version != "v1" {
            return Err(ComposeError::Invalid(format!(
                "Kubernetes apiVersion {api_version:?} is not a core or group API"
            )));
        }
        let kind = document.kind.as_deref().unwrap_or_default();
        let metadata = document.metadata;
        match kind {
            "Pod" | "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job" => {
                let name = named(&metadata, kind)?;
                if services.contains_key(&name) {
                    return Err(ComposeError::Invalid(format!(
                        "Kubernetes declares service {name:?} more than once"
                    )));
                }
                let spec = match kind {
                    "Pod" => document.spec.clone(),
                    _ => document
                        .spec
                        .get("template")
                        .map(|template| template["spec"].clone())
                        .unwrap_or(serde_json::Value::Null),
                };
                let pod_spec = if spec.is_null() {
                    return Err(ComposeError::Invalid(format!(
                        "Kubernetes {kind} {name:?} has no pod spec"
                    )));
                } else {
                    &spec
                };
                let containers = pod_containers(&name, pod_spec)?;
                let pod_manifest = metadata
                    .as_ref()
                    .and_then(|metadata| metadata.annotations.get(MANIFEST_ANNOTATION))
                    .cloned()
                    .unwrap_or_else(|| service_manifest.to_owned());
                let mut translated: Vec<(String, &serde_json::Value, String)> = Vec::new();
                for container in &containers {
                    let service_name = translated_service_name(&name, container, containers.len())?;
                    if container
                        .get("image")
                        .and_then(serde_json::Value::as_str)
                        .is_none()
                    {
                        return Err(ComposeError::Invalid(format!(
                            "Kubernetes service {service_name:?} has no image"
                        )));
                    }
                    translated.push((
                        service_name,
                        container,
                        container_manifest(container, &pod_manifest),
                    ));
                }
                for (service_name, container, manifest) in &translated {
                    let container = *container;
                    let name = service_name;
                    let mut command: Vec<String> = container
                        .get("command")
                        .and_then(serde_json::Value::as_array)
                        .map(|argv| {
                            argv.iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(args) = container.get("args").and_then(serde_json::Value::as_array)
                    {
                        command.extend(
                            args.iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_owned),
                        );
                    }
                    let service_manifest_path = PathBuf::from(manifest);
                    if service_manifest_path.is_absolute() {
                        return Err(ComposeError::Invalid(format!(
                        "service {name:?} manifest {manifest:?} must be relative to the Kubernetes manifest"
                    )));
                    }
                    let read_only = pod_spec
                        .get("securityContext")
                        .and_then(|context| context.get("readOnlyRootFilesystem"))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    // ConfigMap and Secret volumes translate into per-key file
                    // mounts; every other volume type stays rejected by name.
                    let mut volume_sources: BTreeMap<String, PodVolume> = BTreeMap::new();
                    if let Some(volumes) = pod_spec
                        .get("volumes")
                        .and_then(serde_json::Value::as_array)
                    {
                        for volume in volumes {
                            let volume_name = volume
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    ComposeError::Invalid(format!(
                                        "Kubernetes service {name:?} has a volume without a name"
                                    ))
                                })?
                                .to_owned();
                            if let Some(config_map) = volume.get("configMap") {
                                let source = config_map
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    ComposeError::Invalid(format!(
                                        "Kubernetes service {name:?} has a configMap volume without a name"
                                    ))
                                })?
                                .to_owned();
                                volume_sources.insert(
                                    volume_name,
                                    PodVolume {
                                        kind: "config".to_owned(),
                                        source,
                                        items: parse_volume_items(config_map.get("items"))?,
                                    },
                                );
                            } else if let Some(secret) = volume.get("secret") {
                                let source = secret
                                .get("secretName")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    ComposeError::Invalid(format!(
                                        "Kubernetes service {name:?} has a secret volume without a secretName"
                                    ))
                                })?
                                .to_owned();
                                volume_sources.insert(
                                    volume_name,
                                    PodVolume {
                                        kind: "secret".to_owned(),
                                        source,
                                        items: parse_volume_items(secret.get("items"))?,
                                    },
                                );
                            } else {
                                let kind = volume
                                    .as_object()
                                    .and_then(|object| {
                                        object.keys().find(|key| key != &"name").cloned()
                                    })
                                    .unwrap_or_else(|| "unknown".to_owned());
                                return Err(ComposeError::Invalid(format!(
                                "Kubernetes service {name:?} volume {volume_name:?} has type {kind:?}; the supported subset is configMap and secret"
                            )));
                            }
                        }
                    }
                    let service_name = name.clone();
                    services.insert(
                        name.clone(),
                        ComposeService {
                            theseus: ServiceTheseus {
                                manifest: service_manifest_path,
                                faults: Vec::new(),
                                coverage: Vec::new(),
                            },
                            networks: Vec::new(),
                            depends_on: None,
                            environment: environment(container),
                            env_file: None,
                            command: Some(command),
                            entrypoint: None,
                            working_dir: container
                                .get("workingDir")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned),
                            user: container
                                .get("securityContext")
                                .and_then(|context| context.get("runAsUser"))
                                .map(|user| user.to_string()),
                            configs: Vec::new(),
                            secrets: Vec::new(),
                            volumes: Vec::new(),
                            healthcheck: None,
                            hostname: None,
                            extra_hosts: None,
                            cpus: None,
                            mem_limit: None,
                            deploy: None,
                            read_only,
                            tmpfs: Vec::new(),
                        },
                    );
                    if let Some(metadata) = &metadata {
                        labels.insert(service_name.clone(), metadata.labels.clone());
                    }
                    pod_mounts.insert(
                        name.clone(),
                        (
                            container
                                .get("volumeMounts")
                                .and_then(serde_json::Value::as_array)
                                .cloned()
                                .unwrap_or_default(),
                            volume_sources,
                        ),
                    );
                }
            }
            "ConfigMap" => {
                let name = named(&metadata, kind)?;
                let mut entries = InlineEntries::new();
                if let Some(data) = document
                    .data
                    .as_ref()
                    .and_then(serde_json::Value::as_object)
                {
                    for (key, value) in data {
                        let content = value.as_str().ok_or_else(|| {
                            ComposeError::Invalid(format!(
                                "Kubernetes ConfigMap {name:?} entry {key:?} is not a string"
                            ))
                        })?;
                        entries.insert(key.clone(), content.as_bytes().to_vec());
                    }
                }
                // binaryData entries are standard base64, decoded into the
                // same locked file bytes as plain entries.
                if let Some(data) = document
                    .binary_data
                    .as_ref()
                    .and_then(serde_json::Value::as_object)
                {
                    for (key, value) in data {
                        let encoded = value.as_str().ok_or_else(|| {
                            ComposeError::Invalid(format!(
                                "Kubernetes ConfigMap {name:?} binaryData entry {key:?} is not a string"
                            ))
                        })?;
                        let decoded = decode_secret_data(encoded)?;
                        entries.insert(key.clone(), decoded);
                    }
                }
                if entries.is_empty() {
                    return Err(ComposeError::Invalid(format!(
                        "Kubernetes ConfigMap {name:?} carries no data entries"
                    )));
                }
                config_entries.insert(name, entries);
            }
            "Secret" => {
                let name = named(&metadata, kind)?;
                let mut entries = InlineEntries::new();
                if let Some(data) = document
                    .data
                    .as_ref()
                    .and_then(serde_json::Value::as_object)
                {
                    for (key, value) in data {
                        let encoded = value.as_str().ok_or_else(|| {
                            ComposeError::Invalid(format!(
                                "Kubernetes Secret {name:?} entry {key:?} is not a string"
                            ))
                        })?;
                        entries.insert(key.clone(), decode_secret_data(encoded)?);
                    }
                }
                if let Some(data) = document
                    .string_data
                    .as_ref()
                    .and_then(serde_json::Value::as_object)
                {
                    for (key, value) in data {
                        let content = value.as_str().ok_or_else(|| {
                            ComposeError::Invalid(format!(
                                "Kubernetes Secret {name:?} entry {key:?} is not a string"
                            ))
                        })?;
                        entries.insert(key.clone(), content.as_bytes().to_vec());
                    }
                }
                if entries.is_empty() {
                    return Err(ComposeError::Invalid(format!(
                        "Kubernetes Secret {name:?} carries no data entries"
                    )));
                }
                secret_entries.insert(name, entries);
            }
            "Service" => {
                let name = named(&metadata, kind)?;
                let service_type = document
                    .spec
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("ClusterIP");
                if service_type != "ClusterIP" {
                    return Err(ComposeError::Invalid(format!(
                        "Kubernetes Service {name:?} has type {service_type:?}; the supported subset is ClusterIP"
                    )));
                }
                let selector: Vec<(String, String)> = document
                    .spec
                    .get("selector")
                    .and_then(serde_json::Value::as_object)
                    .map(|selector| {
                        selector
                            .iter()
                            .filter_map(|(key, value)| {
                                value.as_str().map(|value| (key.clone(), value.to_owned()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                selectors.insert(format!("k8s-{name}"), selector);
            }
            "" => {
                return Err(ComposeError::Invalid(
                    "Kubernetes document has no kind".to_owned(),
                ))
            }
            other => {
                return Err(ComposeError::Invalid(format!(
                    "Kubernetes kind {other:?} is outside the supported subset: Pods, Deployments, and ClusterIP Services"
                )));
            }
        }
    }

    if services.is_empty() {
        return Err(ComposeError::Invalid(
            "Kubernetes manifest declares no Pods or Deployments".to_owned(),
        ));
    }

    // Materialize per-key file mounts for every pod's volumeMounts: a
    // mountPath plus each entry key is one locked read-only file.
    for (service_name, (mounts, sources)) in &pod_mounts {
        for mount in mounts {
            let volume_name = mount
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "Kubernetes service {service_name:?} has a volumeMount without a name"
                    ))
                })?;
            let mount_path = mount
                .get("mountPath")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "Kubernetes service {service_name:?} volumeMount {volume_name:?} has no mountPath"
                    ))
                })?
                .trim_end_matches('/')
                .to_owned();
            if mount.get("subPath").is_some_and(|value| !value.is_null()) {
                return Err(ComposeError::Invalid(format!(
                    "Kubernetes service {service_name:?} volumeMount {volume_name:?} declares subPath, which is not supported"
                )));
            }
            let Some(volume) = sources.get(volume_name) else {
                return Err(ComposeError::Invalid(format!(
                    "Kubernetes service {service_name:?} volumeMount {volume_name:?} names no declared volume"
                )));
            };
            let entries = match volume.kind.as_str() {
                "config" => config_entries.get(&volume.source),
                _ => secret_entries.get(&volume.source),
            }
            .ok_or_else(|| {
                ComposeError::Invalid(format!(
                    "Kubernetes service {service_name:?} mounts {} {:?}, which the manifest does not define",
                    volume.kind, volume.source
                ))
            })?;
            let service = services.get_mut(service_name).ok_or_else(|| {
                ComposeError::Invalid(format!(
                    "Kubernetes service {service_name:?} disappeared before its mounts"
                ))
            })?;
            if std::env::var("THESEUS_DEBUG_K8S").is_ok() {
                eprintln!(
                    "DEBUG volume {volume_name:?} kind {} source {} entries {:?} items {:?}",
                    volume.kind,
                    volume.source,
                    entries.keys().collect::<Vec<_>>(),
                    volume.items
                );
            }
            // The optional items projection remaps entry keys to relative
            // paths under the mount; without it every entry key mounts at
            // mountPath/key.
            let mounted: Vec<(&str, &Vec<u8>, &str)> = match &volume.items {
                Some(projections) => {
                    let mut projected = Vec::new();
                    for (key, path) in projections {
                        let bytes = entries.get(key).ok_or_else(|| {
                            ComposeError::Invalid(format!(
                                "Kubernetes service {service_name:?} mounts {} {:?} item {key:?}, which the manifest does not define",
                                volume.kind, volume.source
                            ))
                        })?;
                        projected.push((key.as_str(), bytes, path.as_str()));
                    }
                    projected
                }
                None => entries
                    .iter()
                    .map(|(key, bytes)| (key.as_str(), bytes, key.as_str()))
                    .collect(),
            };
            for (key, bytes, relative) in mounted {
                let definition = definition_name(&volume.kind, &volume.source, key);
                let target = format!("{mount_path}/{relative}");
                match volume.kind.as_str() {
                    "config" => configs.insert(definition.clone(), bytes.clone()),
                    _ => secrets.insert(definition.clone(), bytes.clone()),
                };
                service
                    .configs
                    .push(ComposeServiceConfig::Mount(ComposeConfigMount {
                        source: definition,
                        target: Some(target),
                    }));
            }
        }
    }

    // A ClusterIP Service is a named network: every pod whose labels match
    // the selector joins it, and reaches its peers by service name.
    for service in services.values_mut() {
        service.networks.push("default".to_owned());
    }
    for (network, selector) in &selectors {
        if selector.is_empty() {
            continue;
        }
        for (name, pod_labels) in &labels {
            let selected = selector
                .iter()
                .all(|(key, value)| pod_labels.get(key).is_some_and(|pod| pod == value));
            if selected {
                services
                    .get_mut(name)
                    .unwrap()
                    .networks
                    .push(network.clone());
            }
        }
    }

    let mut declared = BTreeMap::new();
    declared.insert("default".to_owned(), ComposeNetwork {});
    for network in selectors {
        declared.insert(network.0, ComposeNetwork {});
    }

    Ok(KubernetesInputs {
        // The ComposeFile carries no config/secret definitions: the bytes
        // flow inline through KubernetesInputs and load_compose_plan seeds
        // them directly, so no host files are read or written.
        compose: ComposeFile {
            name: None,
            services,
            networks: declared,
            configs: BTreeMap::new(),
            secrets: BTreeMap::new(),
            theseus: Some(ComposeTheseus {
                replay_start: Default::default(),
                campaign: None,
            }),
        },
        configs,
        secrets,
    })
}
