// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kubernetes manifest input for the documented supported subset.
//!
//! Theseus services are single-container VMs with one immutable image, one
//! entrypoint contract, and named networks; this module translates the
//! Kubernetes subset that maps onto that model - Pods and Deployments with
//! exactly one container, and ClusterIP Services as network membership -
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
    ComposeConfigDefinition, ComposeConfigMount, ComposeEnvironment, ComposeFile, ComposeNetwork,
    ComposeService, ComposeServiceConfig, ComposeTheseus, ServiceTheseus,
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

fn one_container<'a>(
    service_name: &str,
    spec: &'a serde_json::Value,
) -> Result<&'a serde_json::Value, ComposeError> {
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
    if containers.len() != 1 {
        return Err(ComposeError::Invalid(format!(
            "Kubernetes service {service_name:?} declares {} containers; the supported subset is exactly one",
            containers.len()
        )));
    }
    let container = &containers[0];
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
    Ok(container)
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
    let mut services = BTreeMap::new();
    let mut selectors: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut labels: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut config_entries: BTreeMap<String, InlineEntries> = BTreeMap::new();
    let mut secret_entries: BTreeMap<String, InlineEntries> = BTreeMap::new();
    let mut configs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut secrets: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut pod_mounts: BTreeMap<
        String,
        (Vec<serde_json::Value>, BTreeMap<String, (String, String)>),
    > = BTreeMap::new();

    for document in serde_yaml::Deserializer::from_str(&input) {
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
            "Pod" | "Deployment" => {
                let name = named(&metadata, &kind)?;
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
                let container = one_container(&name, pod_spec)?;
                if container
                    .get("image")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
                {
                    return Err(ComposeError::Invalid(format!(
                        "Kubernetes service {name:?} has no image"
                    )));
                }
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
                if let Some(args) = container.get("args").and_then(serde_json::Value::as_array) {
                    command.extend(
                        args.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned),
                    );
                }
                let manifest = metadata
                    .as_ref()
                    .and_then(|metadata| metadata.annotations.get(MANIFEST_ANNOTATION))
                    .cloned()
                    .unwrap_or_else(|| service_manifest.to_owned());
                let service_manifest_path = PathBuf::from(&manifest);
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
                let mut volume_sources: BTreeMap<String, (String, String)> = BTreeMap::new();
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
                                })?;
                            volume_sources
                                .insert(volume_name, ("config".to_owned(), source.to_owned()));
                        } else if let Some(secret) = volume.get("secret") {
                            let source = secret
                                .get("secretName")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    ComposeError::Invalid(format!(
                                        "Kubernetes service {name:?} has a secret volume without a secretName"
                                    ))
                                })?;
                            volume_sources
                                .insert(volume_name, ("secret".to_owned(), source.to_owned()));
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
            "ConfigMap" => {
                let name = named(&metadata, &kind)?;
                let mut entries = InlineEntries::new();
                if let Some(data) = document
                    .spec
                    .get("data")
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
                if entries.is_empty() {
                    return Err(ComposeError::Invalid(format!(
                        "Kubernetes ConfigMap {name:?} carries no data entries"
                    )));
                }
                config_entries.insert(name, entries);
            }
            "Secret" => {
                let name = named(&metadata, &kind)?;
                let mut entries = InlineEntries::new();
                if let Some(data) = document
                    .spec
                    .get("data")
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
                    .spec
                    .get("stringData")
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
                let name = named(&metadata, &kind)?;
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
            let Some((kind, source)) = sources.get(volume_name) else {
                return Err(ComposeError::Invalid(format!(
                    "Kubernetes service {service_name:?} volumeMount {volume_name:?} names no declared volume"
                )));
            };
            let entries = match kind.as_str() {
                "config" => config_entries.get(source),
                _ => secret_entries.get(source),
            }
            .ok_or_else(|| {
                ComposeError::Invalid(format!(
                    "Kubernetes service {service_name:?} mounts {kind} {source:?}, which the manifest does not define"
                ))
            })?;
            let service = services.get_mut(service_name).ok_or_else(|| {
                ComposeError::Invalid(format!(
                    "Kubernetes service {service_name:?} disappeared before its mounts"
                ))
            })?;
            for (key, bytes) in entries {
                let definition = definition_name(kind, source, key);
                let target = format!("{mount_path}/{key}");
                match kind.as_str() {
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

    // The synthetic definitions are named so `theseus compose plan` output
    // stays auditable, while their bytes flow inline (no host files).
    let mut config_definitions = BTreeMap::new();
    for name in configs.keys() {
        config_definitions.insert(
            name.clone(),
            ComposeConfigDefinition {
                file: Path::new(&format!(".theseus/{name}")).to_path_buf(),
            },
        );
    }
    let mut secret_definitions = BTreeMap::new();
    for name in secrets.keys() {
        secret_definitions.insert(
            name.clone(),
            ComposeConfigDefinition {
                file: Path::new(&format!(".theseus/{name}")).to_path_buf(),
            },
        );
    }

    Ok(KubernetesInputs {
        compose: ComposeFile {
            name: None,
            services,
            networks: declared,
            configs: config_definitions,
            secrets: secret_definitions,
            theseus: Some(ComposeTheseus {
                replay_start: Default::default(),
                campaign: None,
            }),
        },
        configs,
        secrets,
    })
}
