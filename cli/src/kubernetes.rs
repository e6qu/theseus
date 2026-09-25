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
    ComposeEnvironment, ComposeFile, ComposeNetwork, ComposeService, ComposeTheseus, ServiceTheseus,
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
        "volumes",
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
    for unsupported in ["volumeMounts", "ports", "livenessProbe", "readinessProbe"] {
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
/// `service_manifest` is the default Theseus service manifest applied to
/// every translated service, relative to the Kubernetes manifest's
/// directory, overridable per service with the
/// `theseus.io/manifest` annotation.
pub fn load_kubernetes_compose(
    path: impl AsRef<Path>,
    service_manifest: &str,
) -> Result<ComposeFile, ComposeError> {
    let path = path.as_ref();
    let input = std::fs::read_to_string(path).map_err(|source| ComposeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut services = BTreeMap::new();
    let mut selectors: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut labels: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

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

    Ok(ComposeFile {
        name: None,
        services,
        networks: declared,
        configs: BTreeMap::new(),
        secrets: BTreeMap::new(),
        theseus: Some(ComposeTheseus {
            replay_start: Default::default(),
            campaign: None,
        }),
    })
}
