// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Strict Docker Compose-shaped topology input for Theseus guests.
//!
//! Compose is used here only as a familiar topology notation. Theseus does
//! not accept Docker images, host ports, volumes, or host networks: each
//! service points at its own locked Theseus manifest instead.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{load_plan, ArtifactPlan, LoadError, RunPlan};

#[derive(Debug)]
pub enum ComposeError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    Invalid(String),
    Manifest {
        service: String,
        source: Box<LoadError>,
    },
}

impl fmt::Display for ComposeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "cannot parse {}: {source}", path.display())
            }
            Self::Invalid(reason) => write!(formatter, "invalid Compose topology: {reason}"),
            Self::Manifest { service, source } => {
                write!(formatter, "service {service:?}: {source}")
            }
        }
    }
}

impl std::error::Error for ComposeError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeFile {
    #[serde(default)]
    name: Option<String>,
    services: BTreeMap<String, ComposeService>,
    #[serde(default)]
    networks: BTreeMap<String, ComposeNetwork>,
    #[serde(rename = "x-theseus", default)]
    theseus: Option<ComposeTheseus>,
}

/// Topology-wide Theseus configuration.  Keeping campaign input here makes a
/// Compose file the complete description of the system *and* its test
/// campaign; no host-side driver program is required.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeTheseus {
    #[serde(default)]
    campaign: Option<ComposeCampaign>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeCampaign {
    driver: String,
    operations: Vec<ComposeOperation>,
    #[serde(default)]
    faults: Vec<ComposeCampaignFault>,
    #[serde(default)]
    properties: Vec<ComposeProperty>,
    #[serde(default = "default_campaign_runs")]
    max_runs: u16,
    #[serde(default = "default_campaign_faults_per_run")]
    max_faults_per_run: u8,
    #[serde(default = "default_campaign_operations_per_run")]
    max_operations_per_run: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeOperation {
    name: String,
    input: String,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
    #[serde(default)]
    requires_markers: Vec<String>,
    #[serde(default)]
    excludes_markers: Vec<String>,
    #[serde(default)]
    max_uses: Option<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeCampaignFault {
    kind: CampaignFaultKind,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    drive: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    at_round: Option<u64>,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<u64>,
    #[serde(default)]
    error_ppm: Option<u32>,
    #[serde(default)]
    latency_rounds: Option<u32>,
    #[serde(default)]
    torn_write_bytes: Option<u32>,
    #[serde(default)]
    corrupt_read_xor: Option<u8>,
    #[serde(default)]
    ethertype: Option<u16>,
    #[serde(default)]
    ip_protocol: Option<u8>,
    #[serde(default)]
    source_port: Option<u16>,
    #[serde(default)]
    destination_port: Option<u16>,
    #[serde(default)]
    drop_ppm: Option<u32>,
    #[serde(default)]
    duplicate_ppm: Option<u32>,
    #[serde(default)]
    corrupt_ppm: Option<u32>,
    #[serde(default)]
    jitter_rounds: Option<u32>,
    #[serde(default)]
    tx_bytes_per_round: Option<u64>,
    #[serde(default)]
    mtu_bytes: Option<u32>,
    #[serde(default)]
    tx_queue_frames: Option<u32>,
    #[serde(default)]
    rx_queue_frames: Option<u32>,
}

/// Campaign-only faults. Lifecycle faults occur on scheduler rounds; topology
/// actions run immediately after a named operation reports its UART barrier.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignFaultKind {
    Pause,
    Restart,
    ClockJump,
    Partition,
    Heal,
    LinkPartition,
    LinkHeal,
    StorageFault,
    StorageRecover,
    NetworkFault,
    NetworkRecover,
    PacketFault,
    PacketRecover,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeProperty {
    name: String,
    kind: PropertyKind,
    contains: String,
    #[serde(default)]
    service: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyKind {
    /// Every generated timeline must report the property.
    Always,
    /// At least one generated timeline must report the property.
    Sometimes,
    /// The campaign must reach a timeline that reports the property.
    Reachable,
    /// No generated timeline may report the property.
    Unreachable,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeNetwork {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeService {
    #[serde(rename = "x-theseus")]
    theseus: ServiceTheseus,
    #[serde(default)]
    networks: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceTheseus {
    manifest: PathBuf,
    #[serde(default)]
    faults: Vec<ComposeFault>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeFault {
    at_round: u64,
    kind: FaultKind,
    #[serde(default)]
    duration_rounds: Option<u64>,
    #[serde(default)]
    nanoseconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    Pause,
    Restart,
    ClockJump,
}

#[derive(Debug, Clone, Serialize)]
pub struct FaultPlan {
    pub at_round: u64,
    pub kind: FaultKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_rounds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nanoseconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComposePlan {
    pub format: String,
    pub compose: String,
    pub name: Option<String>,
    pub services: BTreeMap<String, ComposeServicePlan>,
    /// Network name to sorted service names. Every member can exchange frames
    /// once the deterministic multi-guest switch is available.
    pub networks: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign: Option<CampaignPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topology_runner: Option<ArtifactPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComposeServicePlan {
    pub manifest: String,
    pub run: RunPlan,
    pub networks: Vec<String>,
    pub faults: Vec<FaultPlan>,
}

/// A deterministic, serial-driven topology campaign.  Operations are UTF-8
/// UART input for the designated workload service.  The same line protocol is
/// usable from a shell or C program; an SDK is optional.
#[derive(Debug, Clone, Serialize)]
pub struct CampaignPlan {
    pub driver: String,
    pub operations: Vec<OperationPlan>,
    pub faults: Vec<CampaignFaultPlan>,
    pub properties: Vec<PropertyPlan>,
    pub max_runs: u16,
    pub max_faults_per_run: u8,
    pub max_operations_per_run: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationPlan {
    pub name: String,
    pub input_hex: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_markers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes_markers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CampaignFaultPlan {
    pub kind: CampaignFaultKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_round: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_rounds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nanoseconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torn_write_bytes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrupt_read_xor: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ethertype: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_protocol: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drop_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrupt_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_bytes_per_round: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu_bytes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_queue_frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_queue_frames: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PropertyPlan {
    pub name: String,
    pub kind: PropertyKind,
    pub contains: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// Load a Compose topology and lock every referenced service artifact into a
/// normalized plan. Relative paths are rooted at the Compose file and may not
/// escape its directory.
pub fn load_compose_plan(path: impl AsRef<Path>) -> Result<ComposePlan, ComposeError> {
    let path = path.as_ref();
    let compose_path = fs::canonicalize(path).map_err(|source| ComposeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let compose_dir = compose_path.parent().expect("canonical path has a parent");
    let input = fs::read_to_string(&compose_path).map_err(|source| ComposeError::Read {
        path: compose_path.clone(),
        source,
    })?;
    let compose: ComposeFile =
        serde_yaml::from_str(&input).map_err(|source| ComposeError::Parse {
            path: compose_path.clone(),
            source,
        })?;

    if compose.services.is_empty() {
        return Err(ComposeError::Invalid(
            "services must not be empty".to_owned(),
        ));
    }
    if compose.networks.is_empty() {
        return Err(ComposeError::Invalid(
            "declare at least one named network; Theseus never uses a host default network"
                .to_owned(),
        ));
    }

    let mut memberships: BTreeMap<String, BTreeSet<String>> = compose
        .networks
        .keys()
        .map(|name| (name.clone(), BTreeSet::new()))
        .collect();
    let mut services = BTreeMap::new();
    for (name, service) in compose.services {
        validate_name("service", &name)?;
        if service.networks.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} must join at least one named network"
            )));
        }
        let mut networks = BTreeSet::new();
        for network in service.networks {
            validate_name("network", &network)?;
            let Some(members) = memberships.get_mut(&network) else {
                return Err(ComposeError::Invalid(format!(
                    "service {name:?} references undeclared network {network:?}"
                )));
            };
            members.insert(name.clone());
            networks.insert(network);
        }
        if service.theseus.manifest.is_absolute() {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} x-theseus.manifest must be relative to the Compose file"
            )));
        }
        let manifest =
            fs::canonicalize(compose_dir.join(&service.theseus.manifest)).map_err(|source| {
                ComposeError::Read {
                    path: compose_dir.join(&service.theseus.manifest),
                    source,
                }
            })?;
        if !manifest.starts_with(compose_dir) {
            return Err(ComposeError::Invalid(format!(
                "service {name:?} x-theseus.manifest must not escape the Compose directory"
            )));
        }
        let run = load_plan(&manifest).map_err(|source| ComposeError::Manifest {
            service: name.clone(),
            source: Box::new(source),
        })?;
        let faults = validate_faults(
            &name,
            service.theseus.faults,
            run.run.virtual_time.is_some(),
        )?;
        services.insert(
            name,
            ComposeServicePlan {
                manifest: manifest.display().to_string(),
                run,
                networks: networks.into_iter().collect(),
                faults,
            },
        );
    }

    let campaign = campaign_plan(compose.theseus, &services)?;
    let networks = memberships
        .into_iter()
        .map(|(name, services)| (name, services.into_iter().collect()))
        .collect();
    Ok(ComposePlan {
        format: "theseus-compose-plan-v1".to_owned(),
        compose: compose_path.display().to_string(),
        name: compose.name,
        services,
        networks,
        campaign,
        topology_runner: None,
    })
}

fn default_campaign_runs() -> u16 {
    32
}

fn default_campaign_faults_per_run() -> u8 {
    2
}

fn default_campaign_operations_per_run() -> u8 {
    3
}

fn campaign_plan(
    campaign: Option<ComposeTheseus>,
    services: &BTreeMap<String, ComposeServicePlan>,
) -> Result<Option<CampaignPlan>, ComposeError> {
    let Some(campaign) = campaign.and_then(|theseus| theseus.campaign) else {
        return Ok(None);
    };
    if !services.contains_key(&campaign.driver) {
        return Err(ComposeError::Invalid(format!(
            "campaign driver {:?} is not a service",
            campaign.driver
        )));
    }
    if campaign.operations.is_empty() {
        return Err(ComposeError::Invalid(
            "campaign operations must not be empty".to_owned(),
        ));
    }
    if campaign.max_runs == 0 || campaign.max_runs > 256 {
        return Err(ComposeError::Invalid(
            "campaign max_runs must be between 1 and 256".to_owned(),
        ));
    }
    if campaign.max_faults_per_run == 0 || campaign.max_faults_per_run > 4 {
        return Err(ComposeError::Invalid(
            "campaign max_faults_per_run must be between 1 and 4".to_owned(),
        ));
    }
    if campaign.max_operations_per_run == 0 || campaign.max_operations_per_run > 4 {
        return Err(ComposeError::Invalid(
            "campaign max_operations_per_run must be between 1 and 4".to_owned(),
        ));
    }
    let mut names = BTreeSet::new();
    let mut operations = Vec::with_capacity(campaign.operations.len());
    for operation in campaign.operations {
        validate_name("campaign operation", &operation.name)?;
        if !names.insert(operation.name.clone()) {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} is declared more than once",
                operation.name
            )));
        }
        if operation.input.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} has empty input",
                operation.name
            )));
        }
        operations.push(OperationPlan {
            name: operation.name,
            input_hex: hex(operation.input.as_bytes()),
            requires: operation.requires,
            excludes: operation.excludes,
            requires_markers: operation.requires_markers,
            excludes_markers: operation.excludes_markers,
            max_uses: operation.max_uses,
        });
    }
    validate_campaign_operation_rules(&operations)?;
    let mut faults = Vec::with_capacity(campaign.faults.len());
    for candidate in campaign.faults {
        let after_is_operation =
            |after: &str| operations.iter().any(|operation| operation.name == after);
        let has_network_conditions = candidate.drop_ppm.is_some()
            || candidate.duplicate_ppm.is_some()
            || candidate.corrupt_ppm.is_some()
            || candidate.jitter_rounds.is_some()
            || candidate.tx_bytes_per_round.is_some()
            || candidate.mtu_bytes.is_some()
            || candidate.tx_queue_frames.is_some()
            || candidate.rx_queue_frames.is_some();
        match candidate.kind {
            CampaignFaultKind::Pause
            | CampaignFaultKind::Restart
            | CampaignFaultKind::ClockJump => {
                let service_name = candidate.service.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign lifecycle fault requires service".to_owned())
                })?;
                let at_round = candidate.at_round.ok_or_else(|| {
                    ComposeError::Invalid("campaign lifecycle fault requires at_round".to_owned())
                })?;
                if candidate.network.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.after.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign lifecycle faults accept only service, at_round, duration_rounds, and nanoseconds"
                            .to_owned(),
                    ));
                }
                let service = services.get(service_name).ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "campaign fault references unknown service {service_name:?}",
                    ))
                })?;
                if service
                    .faults
                    .iter()
                    .any(|fault| fault.at_round == at_round)
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign fault for service {service_name:?} duplicates its fixed fault at round {at_round}",
                    )));
                }
                let kind = match candidate.kind {
                    CampaignFaultKind::Pause => FaultKind::Pause,
                    CampaignFaultKind::Restart => FaultKind::Restart,
                    CampaignFaultKind::ClockJump => FaultKind::ClockJump,
                    _ => unreachable!(),
                };
                let mut validated = validate_faults(
                    service_name,
                    vec![ComposeFault {
                        at_round,
                        kind,
                        duration_rounds: candidate.duration_rounds,
                        nanoseconds: candidate.nanoseconds,
                    }],
                    service.run.run.virtual_time.is_some(),
                )?;
                let fault = validated.pop().expect("one validated campaign fault");
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: None,
                    after: None,
                    at_round: Some(fault.at_round),
                    duration_rounds: fault.duration_rounds,
                    nanoseconds: fault.nanoseconds,
                    error_ppm: None,
                    latency_rounds: None,
                    torn_write_bytes: None,
                    corrupt_read_xor: None,
                    ethertype: None,
                    ip_protocol: None,
                    source_port: None,
                    destination_port: None,
                    drop_ppm: None,
                    duplicate_ppm: None,
                    corrupt_ppm: None,
                    jitter_rounds: None,
                    tx_bytes_per_round: None,
                    mtu_bytes: None,
                    tx_queue_frames: None,
                    rx_queue_frames: None,
                });
            }
            CampaignFaultKind::Partition | CampaignFaultKind::Heal => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign partition/heal action requires network".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign partition/heal action requires after".to_owned(),
                    )
                })?;
                if !after_is_operation(after) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action after references unknown operation {after:?}",
                    )));
                }
                if !services
                    .values()
                    .any(|service| service.networks.iter().any(|name| name == network))
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action references unknown network {network:?}",
                    )));
                }
                if candidate.service.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign partition/heal actions accept only network and after".to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    service: None,
                    network: Some(network.to_owned()),
                    from: None,
                    to: None,
                    drive: None,
                    after: Some(after.to_owned()),
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: None,
                    latency_rounds: None,
                    torn_write_bytes: None,
                    corrupt_read_xor: None,
                    ethertype: None,
                    ip_protocol: None,
                    source_port: None,
                    destination_port: None,
                    drop_ppm: None,
                    duplicate_ppm: None,
                    corrupt_ppm: None,
                    jitter_rounds: None,
                    tx_bytes_per_round: None,
                    mtu_bytes: None,
                    tx_queue_frames: None,
                    rx_queue_frames: None,
                });
            }
            CampaignFaultKind::LinkPartition | CampaignFaultKind::LinkHeal => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_partition/link_heal action requires network".to_owned(),
                    )
                })?;
                let from = candidate.from.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_partition/link_heal action requires from".to_owned(),
                    )
                })?;
                let to = candidate.to.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_partition/link_heal action requires to".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign link_partition/link_heal action requires after".to_owned(),
                    )
                })?;
                if from == to {
                    return Err(ComposeError::Invalid(
                        "campaign directed link action requires distinct from and to services"
                            .to_owned(),
                    ));
                }
                if !after_is_operation(after) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action after references unknown operation {after:?}",
                    )));
                }
                for service_name in [from, to] {
                    let service = services.get(service_name).ok_or_else(|| {
                        ComposeError::Invalid(format!(
                            "campaign directed link action references unknown service {service_name:?}",
                        ))
                    })?;
                    if !service.networks.iter().any(|name| name == network) {
                        return Err(ComposeError::Invalid(format!(
                            "campaign directed link action service {service_name:?} is not on network {network:?}",
                        )));
                    }
                }
                if candidate.service.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                    || has_network_conditions
                {
                    return Err(ComposeError::Invalid(
                        "campaign link_partition/link_heal actions accept only network, from, to, and after"
                            .to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    service: None,
                    network: Some(network.to_owned()),
                    from: Some(from.to_owned()),
                    to: Some(to.to_owned()),
                    drive: None,
                    after: Some(after.to_owned()),
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: None,
                    latency_rounds: None,
                    torn_write_bytes: None,
                    corrupt_read_xor: None,
                    ethertype: None,
                    ip_protocol: None,
                    source_port: None,
                    destination_port: None,
                    drop_ppm: None,
                    duplicate_ppm: None,
                    corrupt_ppm: None,
                    jitter_rounds: None,
                    tx_bytes_per_round: None,
                    mtu_bytes: None,
                    tx_queue_frames: None,
                    rx_queue_frames: None,
                });
            }
            CampaignFaultKind::StorageFault | CampaignFaultKind::StorageRecover => {
                let service_name = candidate.service.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign storage_fault action requires service".to_owned(),
                    )
                })?;
                let drive = candidate.drive.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign storage_fault action requires drive".to_owned())
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid("campaign storage_fault action requires after".to_owned())
                })?;
                if !after_is_operation(after) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action after references unknown operation {after:?}",
                    )));
                }
                let service = services.get(service_name).ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "campaign storage_fault references unknown service {service_name:?}",
                    ))
                })?;
                if !service
                    .run
                    .storage
                    .iter()
                    .any(|storage| storage.id == drive)
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign storage_fault references unknown drive {drive:?} on service {service_name:?}",
                    )));
                }
                let error_ppm = candidate.error_ppm.unwrap_or(0);
                if error_ppm > 1_000_000 {
                    return Err(ComposeError::Invalid(
                        "campaign storage_fault error_ppm must be at most 1000000".to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::StorageFault)
                    && error_ppm == 0
                    && candidate.latency_rounds.unwrap_or(0) == 0
                    && candidate.torn_write_bytes.is_none()
                    && candidate.corrupt_read_xor.is_none()
                {
                    return Err(ComposeError::Invalid(
                        "campaign storage_fault must set error_ppm, latency_rounds, torn_write_bytes, or corrupt_read_xor"
                            .to_owned(),
                    ));
                }
                if candidate.network.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || has_network_conditions
                    || candidate.ethertype.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign storage_fault accepts service, drive, after, error_ppm, latency_rounds, torn_write_bytes, and corrupt_read_xor"
                            .to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::StorageRecover)
                    && (candidate.error_ppm.is_some()
                        || candidate.latency_rounds.is_some()
                        || candidate.torn_write_bytes.is_some()
                        || candidate.corrupt_read_xor.is_some())
                {
                    return Err(ComposeError::Invalid(
                        "campaign storage_recover accepts only service, drive, and after"
                            .to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    service: Some(service_name.to_owned()),
                    network: None,
                    from: None,
                    to: None,
                    drive: Some(drive.to_owned()),
                    after: Some(after.to_owned()),
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: matches!(candidate.kind, CampaignFaultKind::StorageFault)
                        .then_some(error_ppm),
                    latency_rounds: candidate.latency_rounds,
                    torn_write_bytes: candidate.torn_write_bytes,
                    corrupt_read_xor: candidate.corrupt_read_xor,
                    ethertype: None,
                    ip_protocol: None,
                    source_port: None,
                    destination_port: None,
                    drop_ppm: None,
                    duplicate_ppm: None,
                    corrupt_ppm: None,
                    jitter_rounds: None,
                    tx_bytes_per_round: None,
                    mtu_bytes: None,
                    tx_queue_frames: None,
                    rx_queue_frames: None,
                });
            }
            CampaignFaultKind::NetworkFault | CampaignFaultKind::NetworkRecover => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign network_fault/network_recover action requires network".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign network_fault/network_recover action requires after".to_owned(),
                    )
                })?;
                if !after_is_operation(after) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action after references unknown operation {after:?}",
                    )));
                }
                if !services
                    .values()
                    .any(|service| service.networks.iter().any(|name| name == network))
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action references unknown network {network:?}",
                    )));
                }
                if candidate.service.is_some()
                    || candidate.from.is_some()
                    || candidate.to.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.ethertype.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign network_fault/network_recover actions accept only network, after, and packet-condition fields"
                            .to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::NetworkFault)
                    && !has_network_conditions
                    && candidate.latency_rounds.is_none()
                {
                    return Err(ComposeError::Invalid(
                        "campaign network_fault must set one packet-condition field".to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::NetworkRecover)
                    && (has_network_conditions || candidate.latency_rounds.is_some())
                {
                    return Err(ComposeError::Invalid(
                        "campaign network_recover accepts only network and after".to_owned(),
                    ));
                }
                for (name, value) in [
                    ("drop_ppm", candidate.drop_ppm),
                    ("duplicate_ppm", candidate.duplicate_ppm),
                    ("corrupt_ppm", candidate.corrupt_ppm),
                ] {
                    if value.is_some_and(|value| value > 1_000_000) {
                        return Err(ComposeError::Invalid(format!(
                            "campaign network_fault {name} must be at most 1000000"
                        )));
                    }
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    service: None,
                    network: Some(network.to_owned()),
                    from: None,
                    to: None,
                    drive: None,
                    after: Some(after.to_owned()),
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: None,
                    latency_rounds: candidate.latency_rounds,
                    torn_write_bytes: None,
                    corrupt_read_xor: None,
                    ethertype: None,
                    ip_protocol: None,
                    source_port: None,
                    destination_port: None,
                    drop_ppm: candidate.drop_ppm,
                    duplicate_ppm: candidate.duplicate_ppm,
                    corrupt_ppm: candidate.corrupt_ppm,
                    jitter_rounds: candidate.jitter_rounds,
                    tx_bytes_per_round: candidate.tx_bytes_per_round,
                    mtu_bytes: candidate.mtu_bytes,
                    tx_queue_frames: candidate.tx_queue_frames,
                    rx_queue_frames: candidate.rx_queue_frames,
                });
            }
            CampaignFaultKind::PacketFault | CampaignFaultKind::PacketRecover => {
                let network = candidate.network.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign packet_fault/packet_recover action requires network".to_owned(),
                    )
                })?;
                let after = candidate.after.as_deref().ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign packet_fault/packet_recover action requires after".to_owned(),
                    )
                })?;
                let ethertype = candidate.ethertype.ok_or_else(|| {
                    ComposeError::Invalid(
                        "campaign packet_fault/packet_recover action requires ethertype".to_owned(),
                    )
                })?;
                if ethertype < 0x0600 {
                    return Err(ComposeError::Invalid(
                        "campaign packet_fault ethertype must be an Ethernet EtherType (at least 0x0600)"
                            .to_owned(),
                    ));
                }
                if candidate.ip_protocol.is_some() && !matches!(ethertype, 0x0800 | 0x86dd) {
                    return Err(ComposeError::Invalid(
                        "campaign packet protocol selectors require IPv4 (0x0800) or IPv6 (0x86dd) ethertype"
                            .to_owned(),
                    ));
                }
                if (candidate.source_port.is_some() || candidate.destination_port.is_some())
                    && !matches!(candidate.ip_protocol, Some(6 | 17))
                {
                    return Err(ComposeError::Invalid(
                        "campaign packet port selectors require ip_protocol 6 (TCP) or 17 (UDP)"
                            .to_owned(),
                    ));
                }
                if !after_is_operation(after) {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action after references unknown operation {after:?}",
                    )));
                }
                if !services
                    .values()
                    .any(|service| service.networks.iter().any(|name| name == network))
                {
                    return Err(ComposeError::Invalid(format!(
                        "campaign action references unknown network {network:?}",
                    )));
                }
                let directed = match (candidate.from.as_deref(), candidate.to.as_deref()) {
                    (None, None) => None,
                    (Some(from), Some(to)) if from != to => {
                        for service_name in [from, to] {
                            let service = services.get(service_name).ok_or_else(|| {
                                ComposeError::Invalid(format!(
                                    "campaign packet action references unknown service {service_name:?}",
                                ))
                            })?;
                            if !service.networks.iter().any(|name| name == network) {
                                return Err(ComposeError::Invalid(format!(
                                    "campaign packet action service {service_name:?} is not on network {network:?}",
                                )));
                            }
                        }
                        Some((from, to))
                    }
                    _ => {
                        return Err(ComposeError::Invalid(
                            "campaign packet action requires both distinct from and to services"
                                .to_owned(),
                        ));
                    }
                };
                if candidate.service.is_some()
                    || candidate.drive.is_some()
                    || candidate.at_round.is_some()
                    || candidate.duration_rounds.is_some()
                    || candidate.nanoseconds.is_some()
                    || candidate.error_ppm.is_some()
                    || candidate.latency_rounds.is_some()
                    || candidate.torn_write_bytes.is_some()
                    || candidate.corrupt_read_xor.is_some()
                    || candidate.duplicate_ppm.is_some()
                    || candidate.corrupt_ppm.is_some()
                    || candidate.jitter_rounds.is_some()
                    || candidate.tx_bytes_per_round.is_some()
                    || candidate.mtu_bytes.is_some()
                    || candidate.tx_queue_frames.is_some()
                    || candidate.rx_queue_frames.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign packet_fault/packet_recover actions accept network, after, ethertype, drop_ppm, and optional from/to"
                            .to_owned(),
                    ));
                }
                let drop_ppm = candidate.drop_ppm.unwrap_or(0);
                if drop_ppm > 1_000_000 {
                    return Err(ComposeError::Invalid(
                        "campaign packet_fault drop_ppm must be at most 1000000".to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::PacketFault)
                    && candidate.drop_ppm.is_none()
                {
                    return Err(ComposeError::Invalid(
                        "campaign packet_fault requires drop_ppm".to_owned(),
                    ));
                }
                if matches!(candidate.kind, CampaignFaultKind::PacketRecover)
                    && candidate.drop_ppm.is_some()
                {
                    return Err(ComposeError::Invalid(
                        "campaign packet_recover accepts only network, after, and ethertype"
                            .to_owned(),
                    ));
                }
                faults.push(CampaignFaultPlan {
                    kind: candidate.kind,
                    service: None,
                    network: Some(network.to_owned()),
                    from: directed.map(|(from, _)| from.to_owned()),
                    to: directed.map(|(_, to)| to.to_owned()),
                    drive: None,
                    after: Some(after.to_owned()),
                    at_round: None,
                    duration_rounds: None,
                    nanoseconds: None,
                    error_ppm: None,
                    latency_rounds: None,
                    torn_write_bytes: None,
                    corrupt_read_xor: None,
                    ethertype: Some(ethertype),
                    ip_protocol: candidate.ip_protocol,
                    source_port: candidate.source_port,
                    destination_port: candidate.destination_port,
                    drop_ppm: matches!(candidate.kind, CampaignFaultKind::PacketFault)
                        .then_some(drop_ppm),
                    duplicate_ppm: None,
                    corrupt_ppm: None,
                    jitter_rounds: None,
                    tx_bytes_per_round: None,
                    mtu_bytes: None,
                    tx_queue_frames: None,
                    rx_queue_frames: None,
                });
            }
        }
    }
    let mut property_names = BTreeSet::new();
    let mut properties = Vec::with_capacity(campaign.properties.len());
    for property in campaign.properties {
        validate_name("campaign property", &property.name)?;
        if !property_names.insert(property.name.clone()) {
            return Err(ComposeError::Invalid(format!(
                "campaign property {:?} is declared more than once",
                property.name
            )));
        }
        if property.contains.is_empty() {
            return Err(ComposeError::Invalid(format!(
                "campaign property {:?} has an empty contains value",
                property.name
            )));
        }
        if let Some(service) = &property.service {
            if !services.contains_key(service) {
                return Err(ComposeError::Invalid(format!(
                    "campaign property {:?} references unknown service {service:?}",
                    property.name
                )));
            }
        }
        properties.push(PropertyPlan {
            name: property.name,
            kind: property.kind,
            contains: property.contains,
            service: property.service,
        });
    }
    Ok(Some(CampaignPlan {
        driver: campaign.driver,
        operations,
        faults,
        properties,
        max_runs: campaign.max_runs,
        max_faults_per_run: campaign.max_faults_per_run,
        max_operations_per_run: campaign.max_operations_per_run,
    }))
}

fn validate_campaign_operation_rules(operations: &[OperationPlan]) -> Result<(), ComposeError> {
    let names = operations
        .iter()
        .map(|operation| operation.name.as_str())
        .collect::<BTreeSet<_>>();
    for operation in operations {
        let mut requirements = BTreeSet::new();
        for requirement in &operation.requires {
            validate_name("campaign operation requirement", requirement)?;
            if !names.contains(requirement.as_str()) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} requires unknown operation {:?}",
                    operation.name, requirement
                )));
            }
            if !requirements.insert(requirement) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} requires {:?} more than once",
                    operation.name, requirement
                )));
            }
        }
        let mut exclusions = BTreeSet::new();
        for exclusion in &operation.excludes {
            validate_name("campaign operation exclusion", exclusion)?;
            if !names.contains(exclusion.as_str()) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} excludes unknown operation {:?}",
                    operation.name, exclusion
                )));
            }
            if !exclusions.insert(exclusion) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} excludes {:?} more than once",
                    operation.name, exclusion
                )));
            }
            if requirements.contains(exclusion) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} both requires and excludes {:?}",
                    operation.name, exclusion
                )));
            }
        }
        let mut required_markers = BTreeSet::new();
        for marker in &operation.requires_markers {
            validate_name("campaign operation required marker", marker)?;
            if !required_markers.insert(marker) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} requires marker {:?} more than once",
                    operation.name, marker
                )));
            }
        }
        let mut excluded_markers = BTreeSet::new();
        for marker in &operation.excludes_markers {
            validate_name("campaign operation excluded marker", marker)?;
            if !excluded_markers.insert(marker) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} excludes marker {:?} more than once",
                    operation.name, marker
                )));
            }
            if required_markers.contains(marker) {
                return Err(ComposeError::Invalid(format!(
                    "campaign operation {:?} both requires and excludes marker {:?}",
                    operation.name, marker
                )));
            }
        }
        if operation
            .max_uses
            .is_some_and(|maximum| maximum == 0 || maximum > 4)
        {
            return Err(ComposeError::Invalid(format!(
                "campaign operation {:?} max_uses must be between 1 and 4",
                operation.name
            )));
        }
    }
    let mut reachable = BTreeSet::new();
    loop {
        let before = reachable.len();
        for operation in operations {
            if operation
                .requires
                .iter()
                .all(|requirement| reachable.contains(requirement.as_str()))
            {
                reachable.insert(operation.name.as_str());
            }
        }
        if reachable.len() == before {
            break;
        }
    }
    if reachable.len() == operations.len() {
        return Ok(());
    }
    let blocked = operations
        .iter()
        .filter(|operation| !reachable.contains(operation.name.as_str()))
        .map(|operation| operation.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(ComposeError::Invalid(format!(
        "campaign operation requirements cannot reach: {blocked}"
    )))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_faults(
    service: &str,
    faults: Vec<ComposeFault>,
    has_virtual_time: bool,
) -> Result<Vec<FaultPlan>, ComposeError> {
    let mut previous_round = 0;
    let mut paused_until = 0;
    let mut plans = Vec::with_capacity(faults.len());
    for fault in faults {
        if fault.at_round == 0 {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} fault at_round must be greater than zero"
            )));
        }
        if fault.at_round <= previous_round {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} faults must use strictly increasing at_round values"
            )));
        }
        if fault.at_round < paused_until {
            return Err(ComposeError::Invalid(format!(
                "service {service:?} fault at round {} falls within a pause ending at round {paused_until}",
                fault.at_round
            )));
        }
        let plan = match fault.kind {
            FaultKind::Pause => {
                let duration = fault.duration_rounds.ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "service {service:?} pause fault requires duration_rounds"
                    ))
                })?;
                if duration == 0 || fault.nanoseconds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} pause fault requires a positive duration_rounds and no nanoseconds"
                    )));
                }
                paused_until = fault.at_round.checked_add(duration).ok_or_else(|| {
                    ComposeError::Invalid(format!("service {service:?} pause duration overflows"))
                })?;
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::Pause,
                    duration_rounds: Some(duration),
                    nanoseconds: None,
                }
            }
            FaultKind::Restart => {
                if fault.duration_rounds.is_some() || fault.nanoseconds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} restart fault takes no duration_rounds or nanoseconds"
                    )));
                }
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::Restart,
                    duration_rounds: None,
                    nanoseconds: None,
                }
            }
            FaultKind::ClockJump => {
                let nanoseconds = fault.nanoseconds.ok_or_else(|| {
                    ComposeError::Invalid(format!(
                        "service {service:?} clock_jump fault requires nanoseconds"
                    ))
                })?;
                if !has_virtual_time || nanoseconds == 0 || fault.duration_rounds.is_some() {
                    return Err(ComposeError::Invalid(format!(
                        "service {service:?} clock_jump requires virtual_time, positive nanoseconds, and no duration_rounds"
                    )));
                }
                FaultPlan {
                    at_round: fault.at_round,
                    kind: FaultKind::ClockJump,
                    duration_rounds: None,
                    nanoseconds: Some(nanoseconds),
                }
            }
        };
        previous_round = plan.at_round;
        plans.push(plan);
    }
    Ok(plans)
}

fn validate_name(kind: &str, value: &str) -> Result<(), ComposeError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ComposeError::Invalid(format!(
            "{kind} name {value:?} must use only letters, digits, '-' and '_'"
        )));
    }
    Ok(())
}

/// Execute a locked plan with the Linux-only runner shipped beside `theseus`
/// in a published runtime bundle. Planning remains portable because the CLI
/// itself never links the Linux/KVM VMM.
pub fn test_compose(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let mut plan = load_compose_plan(path)?;
    plan.topology_runner = Some(installed_runner_artifact()?);
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "replay output already exists: {}",
            output.display()
        )));
    }
    let plan_file = output.with_extension("topology-plan.json");
    if plan_file.exists() {
        return Err(ComposeError::Invalid(format!(
            "temporary topology plan already exists: {}",
            plan_file.display()
        )));
    }
    fs::write(
        &plan_file,
        serde_json::to_vec_pretty(&plan).map_err(|error| {
            ComposeError::Invalid(format!("cannot encode topology plan: {error}"))
        })?,
    )
    .map_err(|source| ComposeError::Read {
        path: plan_file.clone(),
        source,
    })?;
    execute_topology(&plan_file, &output)?;
    let _ = fs::remove_file(&plan_file);
    Ok(output)
}

/// Execute the topology's declared autonomous campaign.  The command uses the
/// same locked-artifact executor as `compose test`; the runner selects
/// campaign mode from the normalized plan instead of accepting host commands.
pub fn explore_compose(
    path: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let plan = load_compose_plan(&path)?;
    if plan.campaign.is_none() {
        return Err(ComposeError::Invalid(
            "Compose file has no x-theseus.campaign section".to_owned(),
        ));
    }
    test_compose(path, output)
}

/// Re-run a recorded topology using its locked service artifacts.
pub fn replay_compose(
    bundle: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| ComposeError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    let plan = bundle.join("replay-plan.json");
    if !plan.is_file() {
        return Err(ComposeError::Invalid(format!(
            "topology replay has no replay-plan.json: {}",
            bundle.display()
        )));
    }
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "replay output already exists: {}",
            output.display()
        )));
    }
    execute_topology(&plan, &output)?;
    Ok(output)
}

/// Reduce one recorded campaign counterexample into a single locked topology
/// replay.  The Linux runner performs the re-executions because only it owns
/// the deterministic VMM and simulated-switch state.
pub fn minimize_compose_campaign(
    bundle: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<PathBuf, ComposeError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| ComposeError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    let plan = bundle.join("replay-plan.json");
    if !bundle.join("campaign-result.json").is_file() {
        return Err(ComposeError::Invalid(format!(
            "campaign bundle has no campaign-result.json: {}",
            bundle.display()
        )));
    }
    let output = output.as_ref().to_path_buf();
    if output.exists() {
        return Err(ComposeError::Invalid(format!(
            "minimized output already exists: {}",
            output.display()
        )));
    }
    execute_topology_mode(&plan, &output, Some("--minimize"))?;
    Ok(output)
}

fn execute_topology(plan: &Path, output: &Path) -> Result<(), ComposeError> {
    execute_topology_mode(plan, output, None)
}

fn execute_topology_mode(
    plan: &Path,
    output: &Path,
    mode: Option<&str>,
) -> Result<(), ComposeError> {
    let runner: TopologyRunnerPlan = serde_json::from_slice(&fs::read(plan).map_err(|source| {
        ComposeError::Read {
            path: plan.to_path_buf(),
            source,
        }
    })?)
    .map_err(|error| ComposeError::Invalid(format!("cannot parse {}: {error}", plan.display())))?;
    let runner = runner
        .topology_runner
        .as_ref()
        .map(verified_runner)
        .transpose()?
        .ok_or_else(|| ComposeError::Invalid("topology replay has no locked executor; replay it with the published runtime that created it".to_owned()))?;
    let status = Command::new(&runner)
        .arg("--plan")
        .arg(plan)
        .arg("--output")
        .arg(output)
        .args(mode)
        .status()
        .map_err(|error| {
            ComposeError::Invalid(format!("cannot start {}: {error}", runner.display()))
        })?;
    if !status.success() {
        return Err(ComposeError::Invalid(format!(
            "topology runner failed; inspect {}",
            output.display()
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
struct TopologyRunnerPlan {
    #[serde(default)]
    topology_runner: Option<ArtifactPlan>,
}

fn installed_runner() -> Result<PathBuf, ComposeError> {
    let runner = std::env::current_exe()
        .map_err(|error| ComposeError::Invalid(format!("cannot locate theseus binary: {error}")))?
        .parent()
        .map(|directory| directory.join("theseus-topology"))
        .ok_or_else(|| {
            ComposeError::Invalid("theseus binary has no parent directory".to_owned())
        })?;
    if !runner.is_file() {
        return Err(ComposeError::Invalid(format!(
            "missing Linux topology runner beside theseus: {}; use a published Linux runtime bundle",
            runner.display()
        )));
    }
    Ok(runner)
}

fn installed_runner_artifact() -> Result<ArtifactPlan, ComposeError> {
    artifact_for_runner(&installed_runner()?)
}

fn verified_runner(artifact: &ArtifactPlan) -> Result<PathBuf, ComposeError> {
    let path = PathBuf::from(&artifact.path);
    if artifact_for_runner(&path)?.sha256 != artifact.sha256 {
        return Err(ComposeError::Invalid(format!(
            "topology runner digest changed: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn artifact_for_runner(path: &Path) -> Result<ArtifactPlan, ComposeError> {
    let path = fs::canonicalize(path).map_err(|source| ComposeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = fs::read(&path).map_err(|source| ComposeError::Read {
        path: path.clone(),
        source,
    })?;
    Ok(ArtifactPlan {
        path: path.display().to_string(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(compose: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        for service in ["api", "worker"] {
            let root = directory.path().join(service);
            fs::create_dir_all(root.join("runtime")).unwrap();
            fs::create_dir_all(root.join("guest")).unwrap();
            fs::write(root.join("runtime/firecracker"), b"firecracker").unwrap();
            #[cfg(unix)]
            fs::set_permissions(
                root.join("runtime/firecracker"),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
            fs::write(root.join("guest/vmlinux"), b"kernel").unwrap();
            fs::write(root.join("guest/initramfs.cpio"), b"initramfs").unwrap();
            fs::write(
                root.join("theseus.toml"),
                "version = 1\n[runtime]\nfirecracker = 'runtime/firecracker'\n[guest]\nkernel = 'guest/vmlinux'\ninitramfs = 'guest/initramfs.cpio'\n[run]\nseed = 1\nvcpu_count = 1\nmem_size_mib = 128\n[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
            )
            .unwrap();
        }
        fs::write(directory.path().join("compose.yaml"), compose).unwrap();
        directory
    }

    #[test]
    fn locks_service_artifacts_and_links() {
        let directory = fixture(
            "name: example\nservices:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.format, "theseus-compose-plan-v1");
        assert_eq!(plan.networks["backplane"], ["api", "worker"]);
        assert_eq!(plan.services["api"].run.guest.kernel.sha256.len(), 64);
    }

    #[test]
    fn rejects_host_compose_features() {
        let directory = fixture(
            "services:\n  api:\n    image: nginx\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("unknown field `image`"));
    }

    #[test]
    fn rejects_undeclared_network() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [missing]\nnetworks:\n  backplane: {}\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("undeclared network"));
    }

    #[test]
    fn locks_per_service_lifecycle_and_clock_schedule() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      faults:\n        - at_round: 2\n          kind: pause\n          duration_rounds: 3\n        - at_round: 6\n          kind: restart\n        - at_round: 8\n          kind: clock_jump\n          nanoseconds: 1000000000\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        assert_eq!(plan.services["api"].faults.len(), 3);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["services"]["api"]["faults"][2]["kind"], "clock_jump");
    }

    #[test]
    fn rejects_clock_jump_without_virtual_time() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n      faults:\n        - at_round: 1\n          kind: clock_jump\n          nanoseconds: 1\n    networks: [backplane]\nnetworks:\n  backplane: {}\n",
        );
        let manifest = directory.path().join("api/theseus.toml");
        let input = fs::read_to_string(&manifest).unwrap();
        fs::write(
            manifest,
            input.replace(
                "[run.virtual_time]\ntick_ns = 1000000\nexits_per_tick = 10\n",
                "",
            ),
        )
        .unwrap();
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("clock_jump requires virtual_time"));
    }

    #[test]
    fn normalizes_a_serial_driven_topology_campaign() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_runs: 8\n    operations:\n      - name: put\n        input: \"put alpha\\n\"\n      - name: get\n        input: \"get alpha\\n\"\n    faults:\n      - service: worker\n        at_round: 2\n        kind: restart\n    properties:\n      - name: no_data_loss\n        kind: always\n        service: api\n        contains: 'THES:ASSERT:no_data_loss:pass'\n      - name: stale_read_is_reachable\n        kind: reachable\n        contains: 'THES:ASSERT:stale_read:fail'\n",
        );
        let plan = load_compose_plan(directory.path().join("compose.yaml")).unwrap();
        let campaign = plan.campaign.expect("campaign is normalized");
        assert_eq!(campaign.driver, "api");
        assert_eq!(campaign.operations[0].input_hex, "70757420616c7068610a");
        assert_eq!(campaign.faults[0].service.as_deref(), Some("worker"));
        assert_eq!(campaign.max_faults_per_run, 2);
        assert_eq!(campaign.max_operations_per_run, 3);
        assert_eq!(campaign.properties.len(), 2);
    }

    #[test]
    fn normalizes_campaign_operation_requirements() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
      - name: read
        input: "read\n"
        requires: [write]
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert_eq!(campaign.operations[1].requires, vec!["write"]);
    }

    #[test]
    fn normalizes_campaign_operation_state_rules() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
        max_uses: 1
        requires_markers: [booted]
      - name: close
        input: "close\n"
        requires: [write]
        max_uses: 1
      - name: read
        input: "read\n"
        requires: [write]
        excludes: [close]
        excludes_markers: [closed]
"#,
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert_eq!(campaign.operations[0].max_uses, Some(1));
        assert_eq!(campaign.operations[0].requires_markers, vec!["booted"]);
        assert_eq!(campaign.operations[2].excludes, vec!["close"]);
        assert_eq!(campaign.operations[2].excludes_markers, vec!["closed"]);
    }

    #[test]
    fn normalizes_operation_barrier_topology_actions() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: write\n        input: 'write\\n'\n      - name: read\n        input: 'read\\n'\n    faults:\n      - kind: partition\n        network: backplane\n        after: write\n      - kind: link_partition\n        network: backplane\n        from: api\n        to: worker\n        after: write\n      - kind: link_heal\n        network: backplane\n        from: api\n        to: worker\n        after: read\n      - kind: storage_fault\n        service: worker\n        drive: data\n        after: write\n        error_ppm: 1000000\n        torn_write_bytes: 1\n      - kind: storage_recover\n        service: worker\n        drive: data\n        after: read\n      - kind: network_fault\n        network: backplane\n        after: write\n        drop_ppm: 1000000\n        latency_rounds: 3\n      - kind: network_recover\n        network: backplane\n        after: read\n",
        );
        let worker = directory.path().join("worker/theseus.toml");
        let input = fs::read_to_string(&worker).unwrap();
        fs::write(
            worker,
            format!("{input}\n[[storage]]\nid = \"data\"\nsize_mib = 1\n"),
        )
        .unwrap();
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert!(matches!(
            campaign.faults[0].kind,
            CampaignFaultKind::Partition
        ));
        assert_eq!(campaign.faults[0].after.as_deref(), Some("write"));
        assert_eq!(campaign.faults[1].from.as_deref(), Some("api"));
        assert_eq!(campaign.faults[1].to.as_deref(), Some("worker"));
        assert!(matches!(
            campaign.faults[2].kind,
            CampaignFaultKind::LinkHeal
        ));
        assert_eq!(campaign.faults[3].drive.as_deref(), Some("data"));
        assert_eq!(campaign.faults[3].error_ppm, Some(1_000_000));
        assert!(matches!(
            campaign.faults[4].kind,
            CampaignFaultKind::StorageRecover
        ));
        assert!(matches!(
            campaign.faults[5].kind,
            CampaignFaultKind::NetworkFault
        ));
        assert_eq!(campaign.faults[5].drop_ppm, Some(1_000_000));
        assert_eq!(campaign.faults[5].latency_rounds, Some(3));
        assert!(matches!(
            campaign.faults[6].kind,
            CampaignFaultKind::NetworkRecover
        ));
    }

    #[test]
    fn normalizes_a_directed_ethertype_matched_packet_fault_and_recovery() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\n  worker:\n    x-theseus:\n      manifest: worker/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: write\n        input: 'write\\n'\n      - name: retry\n        input: 'retry\\n'\n    faults:\n      - kind: packet_fault\n        network: backplane\n        from: api\n        to: worker\n        after: write\n        ethertype: 0x0800\n        drop_ppm: 1000000\n      - kind: packet_recover\n        network: backplane\n        from: api\n        to: worker\n        after: retry\n        ethertype: 0x0800\n",
        );
        let campaign = load_compose_plan(directory.path().join("compose.yaml"))
            .unwrap()
            .campaign
            .unwrap();
        assert!(matches!(
            campaign.faults[0].kind,
            CampaignFaultKind::PacketFault
        ));
        assert_eq!(campaign.faults[0].ethertype, Some(0x0800));
        assert_eq!(campaign.faults[0].drop_ppm, Some(1_000_000));
        assert_eq!(campaign.faults[0].from.as_deref(), Some("api"));
        assert_eq!(campaign.faults[0].to.as_deref(), Some("worker"));
        assert!(matches!(
            campaign.faults[1].kind,
            CampaignFaultKind::PacketRecover
        ));
        assert_eq!(campaign.faults[1].ethertype, Some(0x0800));
        assert_eq!(campaign.faults[1].drop_ppm, None);
    }

    #[test]
    fn rejects_a_campaign_with_an_unknown_driver() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: missing\n    operations:\n      - name: request\n        input: 'request\\n'\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("campaign driver"));
    }

    #[test]
    fn rejects_an_unbounded_campaign_fault_sequence() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_faults_per_run: 5\n    operations:\n      - name: request\n        input: 'request\\n'\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("max_faults_per_run"));
    }

    #[test]
    fn rejects_an_unbounded_campaign_operation_sequence() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    max_operations_per_run: 5\n    operations:\n      - name: request\n        input: 'request\\n'\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("max_operations_per_run"));
    }

    #[test]
    fn rejects_unknown_or_cyclic_campaign_operation_requirements() {
        let unknown = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: read
        input: "read\n"
        requires: [write]
"#,
        );
        let error = load_compose_plan(unknown.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("requires unknown operation"));

        let cyclic = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
        requires: [read]
      - name: read
        input: "read\n"
        requires: [write]
"#,
        );
        let error = load_compose_plan(cyclic.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("operation requirements cannot reach"));
    }

    #[test]
    fn rejects_invalid_campaign_operation_state_rules() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: write
        input: "write\n"
        max_uses: 0
      - name: read
        input: "read\n"
        requires: [write]
        excludes: [write]
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("max_uses must be between 1 and 4"));
    }

    #[test]
    fn rejects_conflicting_campaign_operation_marker_guards() {
        let directory = fixture(
            r#"services:
  api:
    x-theseus:
      manifest: api/theseus.toml
    networks: [backplane]
networks:
  backplane: {}
x-theseus:
  campaign:
    driver: api
    operations:
      - name: read
        input: "read\n"
        requires_markers: [written]
        excludes_markers: [written]
"#,
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error
            .to_string()
            .contains("both requires and excludes marker"));
    }

    #[test]
    fn rejects_an_invalid_campaign_network_condition() {
        let directory = fixture(
            "services:\n  api:\n    x-theseus:\n      manifest: api/theseus.toml\n    networks: [backplane]\nnetworks:\n  backplane: {}\nx-theseus:\n  campaign:\n    driver: api\n    operations:\n      - name: request\n        input: 'request\\n'\n    faults:\n      - kind: network_fault\n        network: backplane\n        after: request\n        drop_ppm: 1000001\n",
        );
        let error = load_compose_plan(directory.path().join("compose.yaml")).unwrap_err();
        assert!(error.to_string().contains("drop_ppm"));
    }
}
