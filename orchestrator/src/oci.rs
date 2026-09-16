// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! OCI container image → bootable initramfs.
//!
//! `flatten` reads a container image in `docker save` format (a tar of
//! `manifest.json`, a config JSON, and layer tars), applies the layers in
//! order with whiteout handling, and writes a `newc` cpio archive that a
//! stock kernel boots as its initramfs. The archive contains the flattened
//! root filesystem plus two injected files:
//!
//! - `/init` — the static pivot binary (mounts dev/proc/sys, reads the
//!   init spec, and either execs the image entrypoint or evaluates an HTTP
//!   service contract around it),
//! - `/etc/theseus-init.json` — the entrypoint, environment, working
//!   directory, and optional service contract from the image config.
//!
//! The image needs no Theseus code of its own; the pivot is the
//! instrumentation.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use serde::Deserialize;

/// The pivot binary, prebuilt by `pivot/build.sh`.
const PIVOT: &[u8] = include_bytes!("../pivot.bin");

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PivotMetadata {
    pub format: &'static str,
    pub architecture: &'static str,
    pub bytes: usize,
}

/// Exact static executable inserted at `/init` in every flattened image.
pub fn pivot_bytes() -> &'static [u8] {
    PIVOT
}

/// Describe the exact PID-1 bytes embedded in this image adapter.
pub fn pivot_metadata() -> Result<PivotMetadata, OciError> {
    let architecture = match PIVOT.get(0..20) {
        Some(header)
            if header.starts_with(b"\x7fELF")
                && u16::from_le_bytes([header[18], header[19]]) == 62 =>
        {
            "amd64"
        }
        Some(header)
            if header.starts_with(b"\x7fELF")
                && u16::from_le_bytes([header[18], header[19]]) == 183 =>
        {
            "arm64"
        }
        _ => return Err(OciError::Pivot),
    };
    Ok(PivotMetadata {
        format: "theseus-pivot-metadata-v1",
        architecture,
        bytes: PIVOT.len(),
    })
}

/// Errors from image flattening.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum OciError {
    /// I/O error: {0}
    Io(#[from] std::io::Error),
    /// Tar error: {0}
    Tar(String),
    /// Malformed manifest or config JSON: {0}
    Json(String),
    /// Image has no entrypoint or command
    NoEntrypoint,
    /// Embedded pivot is not a supported little-endian Linux ELF executable
    Pivot,
}

/// The boot-relevant part of the image config.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct ImageSpec {
    /// Full command line (Entrypoint + Cmd from the image config).
    pub argv: Vec<String>,
    /// Environment variables (`KEY=value`).
    pub env: Vec<String>,
    /// Working directory.
    pub workdir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<ContainerUser>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmpfs: Vec<String>,
}

/// Numeric credentials for a locked Compose image process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerUser {
    pub uid: u32,
    pub gid: u32,
}

/// Literal launch overrides supplied by a Compose service. The adapter applies
/// them while deriving the initramfs, so replay reuses the exact same
/// entrypoint, command, and working-directory contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerLaunch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<ContainerUser>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tmpfs: Vec<String>,
}

/// A read-only file supplied by Compose rather than the container image.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerConfig {
    pub target: String,
    pub data: Vec<u8>,
}

/// A writable image directory seeded from a locked local Compose bind source.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerVolume {
    pub target: String,
    pub directories: Vec<String>,
    pub files: Vec<ContainerConfig>,
}

/// An argv-only Compose health check evaluated by the injected image pivot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerHealthcheck {
    pub command: Vec<String>,
    pub interval_millis: u64,
    pub retries: u32,
    pub start_period_millis: u64,
}

impl ContainerLaunch {
    pub fn is_empty(&self) -> bool {
        self.command.is_none()
            && self.entrypoint.is_none()
            && self.working_dir.is_none()
            && self.user.is_none()
            && !self.read_only
            && self.tmpfs.is_empty()
    }
}

/// The HTTP contract that the injected pivot evaluates around an image.
///
/// The CLI serializes this into the init specification; keeping it here makes
/// the image adapter the single producer of the pivot's on-disk contract.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct ContainerServiceContract {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub campaign: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<HttpReady>,
    #[serde(default)]
    pub assertions: Vec<HttpAssertion>,
    #[serde(default)]
    pub operations: Vec<HttpOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc_ready: Option<GrpcHealth>,
    #[serde(default)]
    pub grpc_assertions: Vec<GrpcAssertion>,
    #[serde(default)]
    pub grpc_operations: Vec<GrpcOperation>,
    #[serde(default)]
    pub shell_operations: Vec<ShellOperation>,
    /// Guest networking synthesized by the Compose topology runner. The pivot
    /// applies this before starting the image entrypoint, so images need not
    /// carry `ip`, a DHCP client, or Theseus-specific startup code.
    #[serde(default, skip_serializing_if = "ContainerNetwork::is_empty")]
    pub network: ContainerNetwork,
}

/// The deterministic L3 view supplied to one image-backed Compose service.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct ContainerNetwork {
    #[serde(default)]
    pub interfaces: Vec<ContainerNetworkInterface>,
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
    /// Compose hostname, set before the image entrypoint starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

impl ContainerNetwork {
    pub fn is_empty(&self) -> bool {
        self.interfaces.is_empty() && self.hosts.is_empty() && self.hostname.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct ContainerNetworkInterface {
    /// Linux guest interface name, for example `eth0`.
    pub name: String,
    /// IPv4 address in dotted-quad form.
    pub address: String,
    pub prefix_len: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct HttpReady {
    pub url: String,
    pub attempts: u32,
    pub interval_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct HttpAssertion {
    pub name: String,
    pub url: String,
    pub expect_status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_contains: Option<String>,
}

/// One HTTP request issued after service readiness and before assertions.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct HttpOperation {
    pub name: String,
    pub method: HttpMethod,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub expect_status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_contains: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

/// A clear-text gRPC health probe. The pivot uses HTTP/2 prior knowledge and
/// the standard `grpc.health.v1.Health/Check` RPC, so no guest SDK is needed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct GrpcHealth {
    pub url: String,
    pub service: String,
    pub attempts: u32,
    pub interval_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct GrpcAssertion {
    pub name: String,
    pub url: String,
    pub service: String,
    pub expect_status: GrpcServingStatus,
}

/// One standard gRPC health request issued after readiness and before the
/// final assertions. Compose campaigns use the same contract through the
/// injected pivot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct GrpcOperation {
    pub name: String,
    pub url: String,
    pub service: String,
    pub expect_status: GrpcServingStatus,
}

/// An argv command run in the image filesystem after service readiness.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct ShellOperation {
    pub name: String,
    pub command: Vec<String>,
    pub expect_exit: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_contains: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub output_json: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrpcServingStatus {
    Unknown,
    Serving,
    NotServing,
    ServiceUnknown,
}

#[derive(Deserialize)]
struct ManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

#[derive(Deserialize)]
struct ImageConfig {
    config: Option<ImageConfigInner>,
}

#[derive(Deserialize, Default)]
struct ImageConfigInner {
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    workdir: Option<String>,
}

#[derive(Clone)]
enum Entry {
    File(Arc<Vec<u8>>, u32),
    Symlink(String),
    Dir,
}

/// Flatten a `docker save` image tar into (cpio_bytes, image_spec).
pub fn flatten(image_tar: &[u8]) -> Result<(Vec<u8>, ImageSpec), OciError> {
    flatten_with_contracts(image_tar, None, None, None, None, None, None, None, None)
}

/// Flatten an image and inject an optional boot-time service contract.
pub fn flatten_with_service(
    image_tar: &[u8],
    service: Option<&ContainerServiceContract>,
) -> Result<(Vec<u8>, ImageSpec), OciError> {
    flatten_with_contracts(image_tar, service, None, None, None, None, None, None, None)
}

/// Flatten an image and inject optional service and network contracts.
///
/// Networking is deliberately separate from the service contract: a normal
/// image can join a Compose network without opting into readiness checks or
/// Theseus-driven operations.
pub fn flatten_with_service_and_network(
    image_tar: &[u8],
    service: Option<&ContainerServiceContract>,
    network: Option<&ContainerNetwork>,
) -> Result<(Vec<u8>, ImageSpec), OciError> {
    flatten_with_contracts(
        image_tar, service, network, None, None, None, None, None, None,
    )
}

/// Flatten an image and inject optional service, network, image, and local
/// bind-volume contracts. Every override applies to the image entrypoint
/// itself, not just to Theseus-driven shell operations.
pub fn flatten_with_contracts(
    image_tar: &[u8],
    service: Option<&ContainerServiceContract>,
    network: Option<&ContainerNetwork>,
    environment: Option<&BTreeMap<String, String>>,
    launch: Option<&ContainerLaunch>,
    configs: Option<&[ContainerConfig]>,
    secrets: Option<&[ContainerConfig]>,
    volumes: Option<&[ContainerVolume]>,
    healthcheck: Option<&ContainerHealthcheck>,
) -> Result<(Vec<u8>, ImageSpec), OciError> {
    let mut archive = tar::Archive::new(image_tar);

    // Follow the manifest's exact Config/Layers references. Modern Docker
    // exports may place both in extensionless blobs; unrelated index JSON
    // must never replace the selected image's launch configuration.
    let mut members = BTreeMap::<String, Vec<u8>>::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(OciError::Tar(
                "outer image archive members must be regular files".into(),
            ));
        }
        let path = entry.path()?;
        if path.components().any(|part| {
            !matches!(
                part,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        }) {
            return Err(OciError::Tar("unsafe outer image archive path".into()));
        }
        let name = path
            .to_str()
            .ok_or_else(|| OciError::Tar("non-utf8 path".into()))?
            .trim_start_matches("./")
            .to_string();
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if members.insert(name, data).is_some() {
            return Err(OciError::Tar("duplicate outer image archive member".into()));
        }
    }

    let manifest: Vec<ManifestEntry> =
        serde_json::from_slice(members.get("manifest.json").ok_or_else(|| {
            OciError::Json(
                "missing manifest.json; supply a single-image docker save archive".into(),
            )
        })?)
        .map_err(|error| OciError::Json(format!("manifest.json: {error}")))?;
    if manifest.len() != 1 {
        return Err(OciError::Json(
            "docker save archive must contain exactly one image".into(),
        ));
    }
    let manifest = manifest
        .into_iter()
        .next()
        .ok_or_else(|| OciError::Json("empty manifest.json".into()))?;
    let config: ImageConfig =
        serde_json::from_slice(members.get(&manifest.config).ok_or_else(|| {
            OciError::Json(format!(
                "missing referenced image config {}",
                manifest.config
            ))
        })?)
        .map_err(|error| OciError::Json(format!("config {}: {error}", manifest.config)))?;

    // Resolve the entrypoint using Compose's image-launch rules: a command
    // replaces the image Cmd while preserving its Entrypoint; a supplied
    // entrypoint replaces the image Entrypoint and drops the image Cmd.
    let inner = config.config.unwrap_or_default();
    let image_entrypoint = inner.entrypoint.unwrap_or_default();
    let image_command = inner.cmd.unwrap_or_default();
    let (mut argv, command) = if let Some(launch) = launch {
        match &launch.entrypoint {
            Some(entrypoint) => (
                entrypoint.clone(),
                launch.command.clone().unwrap_or_default(),
            ),
            None => (
                image_entrypoint,
                launch.command.clone().unwrap_or(image_command),
            ),
        }
    } else {
        (image_entrypoint, image_command)
    };
    argv.extend(command);
    if argv.is_empty() {
        return Err(OciError::NoEntrypoint);
    }
    let mut spec = ImageSpec {
        argv,
        env: inner.env.clone().unwrap_or_default(),
        workdir: launch
            .and_then(|launch| launch.working_dir.clone())
            .unwrap_or_else(|| inner.workdir.unwrap_or_default()),
        user: launch.and_then(|launch| launch.user.clone()),
        read_only: launch.is_some_and(|launch| launch.read_only),
        tmpfs: launch
            .map(|launch| launch.tmpfs.clone())
            .unwrap_or_default(),
    };
    if let Some(environment) = environment {
        apply_environment(&mut spec.env, environment);
    }

    // Apply layers in order.
    let mut files: BTreeMap<String, Entry> = BTreeMap::new();
    for layer_name in &manifest.layers {
        let layer = members
            .get(layer_name)
            .ok_or_else(|| OciError::Tar(format!("missing layer {layer_name}")))?;
        apply_layer(layer, &mut files)?;
    }
    if let Some(volumes) = volumes {
        for volume in volumes {
            let prefix = format!("{}/", volume.target.trim_end_matches('/'));
            files.retain(|path, _| path != &volume.target && !path.starts_with(&prefix));
            for directory in &volume.directories {
                files.insert(directory.clone(), Entry::Dir);
            }
            for file in &volume.files {
                files.insert(
                    file.target.clone(),
                    Entry::File(Arc::new(file.data.clone()), 0o100644),
                );
            }
        }
    }
    if let Some(configs) = configs {
        for config in configs {
            files.insert(
                config.target.clone(),
                Entry::File(Arc::new(config.data.clone()), 0o100444),
            );
        }
    }
    if let Some(secrets) = secrets {
        for secret in secrets {
            files.insert(
                secret.target.clone(),
                Entry::File(Arc::new(secret.data.clone()), 0o100400),
            );
        }
    }

    // Write the cpio archive.
    let init_spec = serde_json::json!({
        "argv": spec.argv,
        "env": spec.env,
        "workdir": spec.workdir,
        "user": spec.user,
        "read_only": spec.read_only,
        "tmpfs": spec.tmpfs,
        "container_service": service,
        "network": network,
        "healthcheck": healthcheck,
    })
    .to_string();

    let mut out = Vec::new();
    let mut ino: u64 = 1;

    // These injected paths must not be overwritten by original image files.
    files.remove("/init");
    files.remove("/etc/theseus-init.json");

    // The kernel's initramfs unpacker does not create parent directories
    // implicitly: every directory in every path needs an explicit entry.
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (path, entry) in &files {
        if matches!(entry, Entry::Dir) {
            dirs.insert(path.clone());
        }
        let mut parent = std::path::Path::new(path.as_str()).parent();
        while let Some(dir) = parent {
            if dir != std::path::Path::new("/") {
                dirs.insert(dir.to_string_lossy().into_owned());
            }
            parent = dir.parent();
        }
    }
    dirs.insert("/etc".to_string());
    for dir in &dirs {
        cpio_dir(&mut out, &mut ino, dir);
    }

    cpio_file(&mut out, &mut ino, "/init", 0o100755, PIVOT);
    cpio_file(
        &mut out,
        &mut ino,
        "/etc/theseus-init.json",
        0o100644,
        init_spec.as_bytes(),
    );
    let mut links = BTreeMap::<usize, (u64, u64, bool)>::new();
    for entry in files.values() {
        if let Entry::File(data, _) = entry {
            links.entry(Arc::as_ptr(data) as usize).or_default().1 += 1;
        }
    }
    for (path, entry) in &files {
        match entry {
            Entry::File(data, mode) => {
                // Pointer identity groups hard links; inode numbering follows
                // lexical pathname order, never allocator address order.
                let (inode, count, written) = links.get_mut(&(Arc::as_ptr(data) as usize)).unwrap();
                if !*written {
                    ino += 1;
                    *inode = ino;
                }
                let payload: &[u8] = if *written { &[] } else { data.as_slice() };
                cpio_header_links(&mut out, *inode, path, *mode, payload.len() as u64, *count);
                out.extend_from_slice(payload);
                pad4(&mut out);
                *written = true;
            }
            Entry::Symlink(target) => cpio_symlink(&mut out, &mut ino, path, target),
            Entry::Dir => {}
        }
    }
    cpio_trailer(&mut out, &mut ino);

    Ok((out, spec))
}

fn apply_environment(entries: &mut Vec<String>, overrides: &BTreeMap<String, String>) {
    for (key, value) in overrides {
        let entry = format!("{key}={value}");
        if let Some(position) = entries.iter().position(|candidate| {
            candidate
                .split_once('=')
                .is_some_and(|(name, _)| name == key)
        }) {
            entries[position] = entry;
        } else {
            entries.push(entry);
        }
    }
}

fn apply_layer(layer: &[u8], files: &mut BTreeMap<String, Entry>) -> Result<(), OciError> {
    let mut archive = tar::Archive::new(layer);
    let mut pending = Vec::<(String, String)>::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = layer_path(&entry.path()?)?;
        pending.retain(|(name, _)| name != &path);
        let base = path.rsplit('/').next().unwrap_or("");

        // Whiteouts.
        if base == ".wh..wh..opq" {
            let dir = path.trim_end_matches("/.wh..wh..opq").to_string();
            files.retain(|k, _| !k.starts_with(&format!("{dir}/")));
            continue;
        }
        if let Some(name) = base.strip_prefix(".wh.") {
            let dir = path.trim_end_matches(base).trim_end_matches('/');
            let target = format!("{dir}/{name}");
            files.retain(|k, _| k != &target && !k.starts_with(&format!("{target}/")));
            continue;
        }

        let header = entry.header();
        match header.entry_type() {
            tar::EntryType::Regular => {
                let mode = if header.mode()? & 0o111 != 0 {
                    0o100755
                } else {
                    0o100644
                };
                let mut data = Vec::new();
                entry.read_to_end(&mut data)?;
                files.insert(path, Entry::File(Arc::new(data), mode));
            }
            tar::EntryType::Link => {
                let target = layer_path(
                    &header
                        .link_name()?
                        .ok_or_else(|| OciError::Tar("hard link has no target".into()))?,
                )?;
                if let Some(target_entry) = files.get(&target) {
                    if !matches!(target_entry, Entry::File(_, _)) {
                        return Err(OciError::Tar(
                            "only regular-file hard links are supported".into(),
                        ));
                    }
                    files.insert(path, target_entry.clone());
                } else {
                    pending.push((path, target));
                }
            }
            tar::EntryType::Symlink => {
                let target = header
                    .link_name()?
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                files.insert(path, Entry::Symlink(target));
            }
            tar::EntryType::Directory => {
                files.insert(path, Entry::Dir);
            }
            _ => {} // Image devices and fifos are not materialized by this profile.
        }
    }
    while !pending.is_empty() {
        let before = pending.len();
        let mut unresolved = Vec::new();
        for (path, target) in pending {
            match files.get(&target) {
                Some(entry @ Entry::File(_, _)) => {
                    files.insert(path, entry.clone());
                }
                Some(_) => {
                    return Err(OciError::Tar(
                        "only regular-file hard links are supported".into(),
                    ))
                }
                None => unresolved.push((path, target)),
            }
        }
        if unresolved.len() == before {
            return Err(OciError::Tar(
                "unresolved or cyclic image hard links".into(),
            ));
        }
        pending = unresolved;
    }
    Ok(())
}

fn layer_path(path: &std::path::Path) -> Result<String, OciError> {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) => normalized.push(name),
            std::path::Component::CurDir | std::path::Component::RootDir => (),
            _ => return Err(OciError::Tar("image path escapes its root".into())),
        }
    }
    Ok(format!(
        "/{}",
        normalized
            .to_str()
            .ok_or_else(|| OciError::Tar("non-utf8 image path".into()))?
    ))
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn cpio_header(out: &mut Vec<u8>, ino: u64, name: &str, mode: u32, filesize: u64) {
    cpio_header_links(out, ino, name, mode, filesize, 1);
}

fn cpio_header_links(
    out: &mut Vec<u8>,
    ino: u64,
    name: &str,
    mode: u32,
    filesize: u64,
    nlink: u64,
) {
    let namesize = (name.len() + 1) as u64;
    let header = format!(
        "070701{ino:08x}{mode:08x}{uid:08x}{gid:08x}{nlink:08x}{mtime:08x}{filesize:08x}{devmajor:08x}{devminor:08x}{rdevmajor:08x}{rdevminor:08x}{namesize:08x}{check:08x}",
        ino = ino,
        mode = mode,
        uid = 1,
        gid = 1,
        nlink = nlink,
        mtime = 0,
        filesize = filesize,
        devmajor = 0,
        devminor = 0,
        rdevmajor = 0,
        rdevminor = 0,
        namesize = namesize,
        check = 0,
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    pad4(out);
}

fn cpio_file(out: &mut Vec<u8>, ino: &mut u64, name: &str, mode: u32, data: &[u8]) {
    *ino += 1;
    cpio_header(out, *ino, name, mode, data.len() as u64);
    out.extend_from_slice(data);
    pad4(out);
}

fn cpio_dir(out: &mut Vec<u8>, ino: &mut u64, name: &str) {
    cpio_file(out, ino, name, 0o040755, &[]);
}

fn cpio_symlink(out: &mut Vec<u8>, ino: &mut u64, name: &str, target: &str) {
    cpio_file(out, ino, name, 0o120777, target.as_bytes());
}

fn cpio_trailer(out: &mut Vec<u8>, ino: &mut u64) {
    cpio_file(out, ino, "TRAILER!!!", 0, &[]);
    while out.len() % 512 != 0 {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn test_image() -> Vec<u8> {
        let layer1 = tar_bytes(&[("app/hello.txt", b"hello")]);
        let layer2 = tar_bytes(&[("app/.wh.hello.txt", b""), ("bin/tool", b"\x7fELF-tool")]);
        let manifest = serde_json::json!([{
            "Config": "config.json",
            "RepoTags": ["test:latest"],
            "Layers": ["l1.tar", "l2.tar"],
        }])
        .to_string();
        let config = serde_json::json!({
            "config": {
                "Entrypoint": ["/bin/tool"],
                "Env": ["A=1"],
                "WorkingDir": "/app",
            }
        })
        .to_string();
        tar_bytes(&[
            ("manifest.json", manifest.as_bytes()),
            ("config.json", config.as_bytes()),
            ("l1.tar", &layer1),
            ("l2.tar", &layer2),
        ])
    }

    #[test]
    fn test_flatten_applies_layers_and_whiteouts() {
        let (cpio, spec) = flatten(&test_image()).unwrap();
        assert_eq!(spec.argv, vec!["/bin/tool".to_string()]);
        assert_eq!(spec.env, vec!["A=1".to_string()]);
        assert_eq!(spec.workdir, "/app");

        let text = String::from_utf8_lossy(&cpio);
        assert!(text.contains("/init"));
        assert!(text.contains("/etc/theseus-init.json"));
        assert!(text.contains("/bin/tool"));
        assert!(
            !text.contains("hello.txt"),
            "whiteout must remove the lower-layer file"
        );
    }

    #[test]
    fn manifest_selects_extensionless_config_and_layers_not_index_json() {
        let layer = tar_bytes(&[("www/health", b"ok\n")]);
        let manifest = serde_json::json!([{"Config": "blobs/sha256/config", "Layers": ["blobs/sha256/layer"]}]).to_string();
        let config = br#"{"config":{"Cmd":["httpd","-f","-p","8080"],"Env":["MODE=service"]}}"#;
        for index_first in [false, true] {
            let mut entries: Vec<(&str, &[u8])> = vec![
                ("manifest.json", manifest.as_bytes()),
                ("blobs/sha256/config", config),
                ("blobs/sha256/layer", &layer),
            ];
            if index_first {
                entries.insert(0, ("index.json", b"{\"schemaVersion\":2}"));
            } else {
                entries.push(("index.json", b"{\"schemaVersion\":2}"));
            }
            let (cpio, spec) = flatten(&tar_bytes(&entries)).unwrap();
            assert_eq!(spec.argv, ["httpd", "-f", "-p", "8080"]);
            assert_eq!(spec.env, ["MODE=service"]);
            assert!(String::from_utf8_lossy(&cpio).contains("/www/health"));
        }
    }

    #[test]
    fn missing_referenced_config_and_ambiguous_images_are_rejected() {
        let missing = br#"[{"Config":"missing","Layers":[]}]"#;
        assert!(flatten(&tar_bytes(&[
            ("manifest.json", missing),
            ("other.json", br#"{"config":{"Cmd":["wrong"]}}"#)
        ]))
        .is_err());
        let multiple = br#"[{"Config":"one","Layers":[]},{"Config":"two","Layers":[]}]"#;
        assert!(flatten(&tar_bytes(&[("manifest.json", multiple)])).is_err());
        assert!(flatten(&tar_bytes(&[
            ("manifest.json", missing),
            ("manifest.json", missing)
        ]))
        .is_err());
    }

    fn linked_layer(forward: bool) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let file = |builder: &mut tar::Builder<Vec<u8>>| {
            let mut header = tar::Header::new_gnu();
            header.set_size(65536);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "bin/[", &vec![42; 65536][..])
                .unwrap();
        };
        if !forward {
            file(&mut builder);
        }
        for (name, target) in [("bin/busybox", "bin/["), ("bin/httpd", "bin/busybox")] {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Link);
            header.set_link_name(target).unwrap();
            header.set_cksum();
            builder.append_data(&mut header, name, &[][..]).unwrap();
        }
        if forward {
            file(&mut builder);
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn busybox_hard_links_share_inode_and_payload_in_cpio() {
        for forward in [false, true] {
            let manifest = br#"[{"Config":"config","Layers":["layer"]}]"#;
            let config = br#"{"config":{"Cmd":["httpd"]}}"#;
            let (cpio, _) = flatten(&tar_bytes(&[
                ("manifest.json", manifest),
                ("config", config),
                ("layer", &linked_layer(forward)),
            ]))
            .unwrap();
            let mut offset = 0;
            let mut inode = None;
            let mut aliases = 0;
            let mut payloads = 0;
            while &cpio[offset..offset + 6] == b"070701" {
                let field = |start, end| {
                    usize::from_str_radix(
                        std::str::from_utf8(&cpio[offset + start..offset + end]).unwrap(),
                        16,
                    )
                    .unwrap()
                };
                let ino = field(6, 14);
                let mode = field(14, 22);
                let count = field(38, 46);
                let bytes = field(54, 62);
                let namesize = field(94, 102);
                let name =
                    std::str::from_utf8(&cpio[offset + 110..offset + 110 + namesize - 1]).unwrap();
                if name == "TRAILER!!!" {
                    break;
                }
                if matches!(name, "/bin/[" | "/bin/busybox" | "/bin/httpd") {
                    assert_eq!(mode, 0o100755);
                    assert_eq!(count, 3);
                    assert_eq!(*inode.get_or_insert(ino), ino);
                    aliases += 1;
                    if bytes > 0 {
                        assert_eq!(bytes, 65536);
                        payloads += 1;
                    }
                }
                offset = (offset + 110 + namesize + 3) & !3;
                offset = (offset + bytes + 3) & !3;
            }
            assert_eq!((aliases, payloads), (3, 1));
        }
    }

    #[test]
    fn hard_link_aliases_survive_target_whiteout_and_replacement() {
        let mut files = BTreeMap::new();
        apply_layer(&linked_layer(false), &mut files).unwrap();
        apply_layer(
            &tar_bytes(&[("bin/.wh.[", &[]), ("bin/[", b"new")]),
            &mut files,
        )
        .unwrap();
        let Entry::File(original, _) = &files["/bin/["] else {
            panic!()
        };
        let Entry::File(alias, _) = &files["/bin/httpd"] else {
            panic!()
        };
        assert_eq!(original.as_slice(), b"new");
        assert_eq!(alias.len(), 65536);
        assert!(!Arc::ptr_eq(original, alias));
        assert!(layer_path(std::path::Path::new("../../outside")).is_err());
    }

    #[test]
    fn image_files_cannot_replace_the_injected_pivot() {
        let manifest = br#"[{"Config":"config","Layers":["layer"]}]"#;
        let config = br#"{"config":{"Cmd":["sh"]}}"#;
        let layer = tar_bytes(&[
            ("init", b"OVERRIDE-PID1"),
            ("etc/theseus-init.json", b"OVERRIDE-CONTRACT"),
        ]);
        let (cpio, _) = flatten(&tar_bytes(&[
            ("manifest.json", manifest),
            ("config", config),
            ("layer", &layer),
        ]))
        .unwrap();
        assert!(!String::from_utf8_lossy(&cpio).contains("OVERRIDE-"));
    }

    #[test]
    fn test_cpio_is_parseable() {
        let (cpio, _) = flatten(&test_image()).unwrap();
        assert!(cpio.starts_with(b"070701"));
        assert!(cpio.windows(10).any(|w| w == b"TRAILER!!!"));
        assert_eq!(cpio.len() % 512, 0);
    }

    #[test]
    fn reports_the_exact_embedded_pivot() {
        let metadata = pivot_metadata().unwrap();
        assert!(matches!(metadata.architecture, "amd64" | "arm64"));
        assert_eq!(metadata.bytes, PIVOT.len());
        assert_eq!(pivot_bytes(), PIVOT);
    }

    #[test]
    fn flatten_injects_the_container_service_contract() {
        let service = ContainerServiceContract {
            campaign: false,
            ready: Some(HttpReady {
                url: "http://127.0.0.1:8080/health".to_owned(),
                attempts: 3,
                interval_millis: 10,
            }),
            assertions: vec![HttpAssertion {
                name: "health".to_owned(),
                url: "http://127.0.0.1:8080/health".to_owned(),
                expect_status: 200,
                body_contains: Some("ok".to_owned()),
            }],
            operations: vec![HttpOperation {
                name: "create".to_owned(),
                method: HttpMethod::Post,
                url: "http://127.0.0.1:8080/items".to_owned(),
                body: Some("item".to_owned()),
                expect_status: 201,
                body_contains: Some("created".to_owned()),
            }],
            grpc_ready: Some(GrpcHealth {
                url: "http://127.0.0.1:50051".to_owned(),
                service: "example.Api".to_owned(),
                attempts: 3,
                interval_millis: 10,
            }),
            grpc_assertions: vec![GrpcAssertion {
                name: "grpc_health".to_owned(),
                url: "http://127.0.0.1:50051".to_owned(),
                service: String::new(),
                expect_status: GrpcServingStatus::Serving,
            }],
            grpc_operations: vec![GrpcOperation {
                name: "grpc_operation".to_owned(),
                url: "http://127.0.0.1:50051".to_owned(),
                service: "example.Api".to_owned(),
                expect_status: GrpcServingStatus::Serving,
            }],
            shell_operations: vec![ShellOperation {
                name: "read_health".to_owned(),
                command: vec!["/bin/cat".to_owned(), "/health".to_owned()],
                expect_exit: 0,
                output_contains: Some("ok".to_owned()),
                output_json: true,
                environment: BTreeMap::from([("CHECK_MODE".to_owned(), "full".to_owned())]),
            }],
            network: ContainerNetwork {
                interfaces: vec![ContainerNetworkInterface {
                    name: "eth0".to_owned(),
                    address: "10.1.0.10".to_owned(),
                    prefix_len: 24,
                }],
                hosts: BTreeMap::from([("worker".to_owned(), "10.1.0.11".to_owned())]),
                hostname: Some("api.local".to_owned()),
            },
        };
        let (cpio, _) = flatten_with_service(&test_image(), Some(&service)).unwrap();
        let text = String::from_utf8_lossy(&cpio);
        assert!(text.contains("container_service"));
        assert!(text.contains("127.0.0.1:8080/health"));
        assert!(text.contains("body_contains"));
        assert!(text.contains("operations"));
        assert!(text.contains("grpc_assertions"));
        assert!(text.contains("grpc_operations"));
        assert!(text.contains("shell_operations"));
        assert!(text.contains("output_json"));
        assert!(text.contains("CHECK_MODE"));
        assert!(text.contains("10.1.0.10"));
        assert!(text.contains("worker"));
        assert!(text.contains("api.local"));
    }

    #[test]
    fn flatten_injects_network_without_a_service_contract() {
        let network = ContainerNetwork {
            interfaces: vec![ContainerNetworkInterface {
                name: "eth0".to_owned(),
                address: "10.1.0.10".to_owned(),
                prefix_len: 24,
            }],
            hosts: BTreeMap::from([("worker".to_owned(), "10.1.0.11".to_owned())]),
            hostname: Some("api.local".to_owned()),
        };
        let (cpio, _) =
            flatten_with_service_and_network(&test_image(), None, Some(&network)).unwrap();
        let text = String::from_utf8_lossy(&cpio);
        assert!(text.contains("\"container_service\":null"));
        assert!(text.contains("\"network\""));
        assert!(text.contains("10.1.0.10"));
    }

    #[test]
    fn flatten_applies_literal_environment_to_the_image_entrypoint() {
        let environment = BTreeMap::from([
            ("MODE".to_owned(), "campaign".to_owned()),
            ("NEW_VALUE".to_owned(), "present".to_owned()),
        ]);
        let (cpio, spec) = flatten_with_contracts(
            &test_image(),
            None,
            None,
            Some(&environment),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(spec.env.iter().any(|entry| entry == "MODE=campaign"));
        assert!(spec.env.iter().any(|entry| entry == "NEW_VALUE=present"));
        assert!(String::from_utf8_lossy(&cpio).contains("MODE=campaign"));
    }

    #[test]
    fn flatten_applies_compose_launch_overrides_to_the_image_entrypoint() {
        let launch = ContainerLaunch {
            command: Some(vec![".".to_owned()]),
            entrypoint: Some(vec![
                "/bin/serve".to_owned(),
                "--port".to_owned(),
                "8080".to_owned(),
            ]),
            working_dir: Some("/site".to_owned()),
            user: Some(ContainerUser {
                uid: 1000,
                gid: 1000,
            }),
            read_only: false,
            tmpfs: Vec::new(),
        };
        let (cpio, spec) = flatten_with_contracts(
            &test_image(),
            None,
            None,
            None,
            Some(&launch),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            spec.argv,
            ["/bin/serve", "--port", "8080", "."].map(str::to_owned)
        );
        assert_eq!(spec.workdir, "/site");
        assert_eq!(
            spec.user,
            Some(ContainerUser {
                uid: 1000,
                gid: 1000
            })
        );
        let text = String::from_utf8_lossy(&cpio);
        assert!(text.contains("/bin/serve"));
        assert!(text.contains("\"workdir\":\"/site\""));
        assert!(text.contains("\"user\""));
        assert!(text.contains("\"uid\":1000"));
        assert!(text.contains("\"gid\":1000"));

        let command_only = ContainerLaunch {
            command: Some(vec!["--foreground".to_owned()]),
            ..ContainerLaunch::default()
        };
        let (_, spec) = flatten_with_contracts(
            &test_image(),
            None,
            None,
            None,
            Some(&command_only),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(spec.argv, ["/bin/tool", "--foreground"].map(str::to_owned));
    }

    #[test]
    fn flatten_overrides_an_image_file_with_a_read_only_compose_config() {
        let configs = [ContainerConfig {
            target: "/app/hello.txt".to_owned(),
            data: b"configured\n".to_vec(),
        }];
        let (cpio, _) = flatten_with_contracts(
            &test_image(),
            None,
            None,
            None,
            None,
            Some(&configs),
            None,
            None,
            None,
        )
        .unwrap();
        let text = String::from_utf8_lossy(&cpio);
        assert!(text.contains("configured\n"));
        assert!(!text.contains("hello\0"));
    }

    #[test]
    fn flatten_overrides_an_image_file_with_a_root_only_compose_secret() {
        let secrets = [ContainerConfig {
            target: "/run/secrets/token".to_owned(),
            data: b"secret\n".to_vec(),
        }];
        let (cpio, _) = flatten_with_contracts(
            &test_image(),
            None,
            None,
            None,
            None,
            None,
            Some(&secrets),
            None,
            None,
        )
        .unwrap();
        let text = String::from_utf8_lossy(&cpio);
        assert!(text.contains("/run/secrets/token"));
        assert!(text.contains("secret\n"));
        assert!(text.contains("00008100"));
    }

    #[test]
    fn flatten_replaces_an_image_directory_with_a_writable_compose_volume() {
        let volumes = [ContainerVolume {
            target: "/app".to_owned(),
            directories: vec!["/app".to_owned(), "/app/state".to_owned()],
            files: vec![ContainerConfig {
                target: "/app/state/value".to_owned(),
                data: b"seeded\n".to_vec(),
            }],
        }];
        let (cpio, _) = flatten_with_contracts(
            &test_image(),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&volumes),
            None,
        )
        .unwrap();
        let text = String::from_utf8_lossy(&cpio);
        assert!(text.contains("/app/state/value"));
        assert!(text.contains("seeded\n"));
        assert!(!text.contains("hello\0"));
    }

    #[test]
    fn flatten_injects_a_compose_healthcheck() {
        let healthcheck = ContainerHealthcheck {
            command: vec!["/bin/check".to_owned(), "--ready".to_owned()],
            interval_millis: 500,
            retries: 4,
            start_period_millis: 1_000,
        };
        let (cpio, _) = flatten_with_contracts(
            &test_image(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&healthcheck),
        )
        .unwrap();
        let text = String::from_utf8_lossy(&cpio);
        assert!(text.contains("\"healthcheck\""));
        assert!(text.contains("/bin/check"));
        assert!(text.contains("\"retries\":4"));
    }

    /// Full image→VM path: build a tiny image containing a static payload
    /// binary, flatten it to an initramfs, boot it, and read the payload's
    /// output on the serial console. Requires KVM.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_boot_container_image() {
        use std::process::Command;

        // 1. Build a tiny static payload that prints and powers off.
        let dir = std::env::temp_dir().join("theseus-oci-test");
        std::fs::create_dir_all(&dir).unwrap();
        let payload_c = dir.join("payload.c");
        std::fs::write(
            &payload_c,
            r#"
#include <stdio.h>
#include <sys/reboot.h>
#include <linux/reboot.h>
int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("CONTAINER-PAYLOAD-OK\n");
    reboot(LINUX_REBOOT_CMD_POWER_OFF);
    return 0;
}
"#,
        )
        .unwrap();
        let payload = dir.join("payload");
        let status = Command::new("cc")
            .args(["-static", "-O2", "-o"])
            .arg(&payload)
            .arg(&payload_c)
            .status()
            .expect("cc not available");
        assert!(status.success(), "payload build failed");

        // 2. Pack a docker-save-format image containing the payload.
        let payload_bytes = std::fs::read(&payload).unwrap();
        let mut layer_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(payload_bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        layer_builder
            .append_data(&mut header, "bin/payload", &payload_bytes[..])
            .unwrap();
        let layer = layer_builder.into_inner().unwrap();

        let manifest = serde_json::json!([{
            "Config": "config.json",
            "RepoTags": ["theseus-test:latest"],
            "Layers": ["layer.tar"],
        }])
        .to_string();
        let config = serde_json::json!({
            "config": { "Entrypoint": ["/bin/payload"] }
        })
        .to_string();
        let mut image_builder = tar::Builder::new(Vec::new());
        for (name, data) in [
            ("manifest.json", manifest.into_bytes()),
            ("config.json", config.into_bytes()),
            ("layer.tar", layer),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            image_builder
                .append_data(&mut header, name, &data[..])
                .unwrap();
        }
        let image_tar = image_builder.into_inner().unwrap();

        // 3. Flatten to initramfs and boot it with the CI kernel.
        let (initramfs, _spec) = flatten(&image_tar).unwrap();
        let initramfs_path = dir.join("initramfs.cpio");
        std::fs::write(&initramfs_path, &initramfs).unwrap();

        let kernel = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../e2e/vmlinux");
        if !kernel.exists() {
            // Download the CI guest kernel (same artifact as e2e/run.sh).
            let status = Command::new("curl")
                .args([
                    "-sSL",
                    "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.9/aarch64/vmlinux-5.10.225",
                    "-o",
                ])
                .arg(&kernel)
                .status()
                .expect("curl not available");
            assert!(status.success(), "kernel download failed");
        }

        let serial = std::env::temp_dir().join("theseus-oci-serial.log");
        let _ = std::fs::remove_file(&serial);

        let resources: vmm::resources::VmResources =
            vmm::test_utils::mock_resources::MockVmResources::new()
                .with_boot_source(vmm::vmm_config::boot_source::BootSourceConfig {
                    kernel_image_path: kernel.to_str().unwrap().to_string(),
                    initrd_path: Some(initramfs_path.to_str().unwrap().to_string()),
                    boot_args: Some("console=ttyS0 reboot=k panic=-1".to_string()),
                })
                .into();
        let mut resources = resources;
        resources.serial_out_path = Some(serial.clone());

        let mut event_manager = vmm::EventManager::new().unwrap();
        let seccomp_filters = vmm::seccomp::get_empty_filters();
        let vmm = vmm::builder::build_microvm_for_boot(
            &vmm::vmm_config::instance_info::InstanceInfo::default(),
            &resources,
            &mut event_manager,
            &seccomp_filters,
        )
        .unwrap();
        vmm.lock().unwrap().resume_vm().unwrap();
        for _ in 0..120 {
            let _ = event_manager.run_with_timeout(500);
            if let Ok(log) = std::fs::read_to_string(&serial) {
                if log.contains("CONTAINER-PAYLOAD-OK") || log.contains("reboot: Power down") {
                    break;
                }
            }
        }
        vmm.lock().unwrap().stop(vmm::FcExitCode::Ok);

        let serial_log = std::fs::read_to_string(&serial).unwrap_or_default();
        assert!(
            serial_log.contains("CONTAINER-PAYLOAD-OK"),
            "payload did not run; serial log tail: {}",
            &serial_log[serial_log.len().saturating_sub(2000)..]
        );
    }
}
