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
//!   init spec, reports a boot marker over the serial control channel,
//!   and execs the image's entrypoint),
//! - `/etc/theseus-init.json` — the entrypoint, environment, and working
//!   directory from the image config.
//!
//! The image needs no Theseus code of its own; the pivot is the
//! instrumentation.

use std::collections::BTreeMap;
use std::io::Read;

use serde::Deserialize;

/// The pivot binary, prebuilt by `pivot/build.sh`.
const PIVOT: &[u8] = include_bytes!("../pivot.bin");

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

enum Entry {
    File(Vec<u8>, u32),
    Symlink(String),
    Dir,
}

/// Flatten a `docker save` image tar into (cpio_bytes, image_spec).
pub fn flatten(image_tar: &[u8]) -> Result<(Vec<u8>, ImageSpec), OciError> {
    let mut archive = tar::Archive::new(image_tar);

    let mut manifest: Vec<ManifestEntry> = Vec::new();
    let mut config: Option<ImageConfig> = None;
    let mut layer_tars: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .to_str()
            .ok_or_else(|| OciError::Tar("non-utf8 path".into()))?
            .to_string();
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if name == "manifest.json" {
            manifest = serde_json::from_slice(&data)
                .map_err(|e| OciError::Json(format!("manifest.json: {e}")))?;
        } else if name.ends_with(".json") && !name.contains("manifest") {
            // The image config JSON.
            config = Some(
                serde_json::from_slice(&data)
                    .map_err(|e| OciError::Json(format!("config {name}: {e}")))?,
            );
        } else if name.ends_with(".tar") {
            layer_tars.insert(name, data);
        }
    }

    let manifest = manifest
        .into_iter()
        .next()
        .ok_or_else(|| OciError::Json("empty manifest.json".into()))?;
    let config = config.ok_or_else(|| OciError::Json("no image config JSON".into()))?;

    // Resolve the entrypoint.
    let inner = config.config.unwrap_or_default();
    let mut argv = inner.entrypoint.clone().unwrap_or_default();
    argv.extend(inner.cmd.clone().unwrap_or_default());
    if argv.is_empty() {
        return Err(OciError::NoEntrypoint);
    }
    let spec = ImageSpec {
        argv,
        env: inner.env.clone().unwrap_or_default(),
        workdir: inner.workdir.clone().unwrap_or_default(),
    };

    // Apply layers in order.
    let mut files: BTreeMap<String, Entry> = BTreeMap::new();
    for layer_name in &manifest.layers {
        let layer = layer_tars
            .get(layer_name)
            .ok_or_else(|| OciError::Tar(format!("missing layer {layer_name}")))?;
        apply_layer(layer, &mut files)?;
    }

    // Write the cpio archive.
    let init_spec = serde_json::json!({
        "argv": spec.argv,
        "env": spec.env,
        "workdir": spec.workdir,
    })
    .to_string();

    let mut out = Vec::new();
    let mut ino: u64 = 1;

    // The kernel's initramfs unpacker does not create parent directories
    // implicitly: every directory in every path needs an explicit entry.
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for path in files.keys() {
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

    cpio_file(
        &mut out,
        &mut ino,
        "/init",
        0o100755,
        PIVOT,
    );
    cpio_file(
        &mut out,
        &mut ino,
        "/etc/theseus-init.json",
        0o100644,
        init_spec.as_bytes(),
    );
    for (path, entry) in &files {
        match entry {
            Entry::File(data, mode) => cpio_file(&mut out, &mut ino, path, *mode, data),
            Entry::Symlink(target) => cpio_symlink(&mut out, &mut ino, path, target),
            Entry::Dir => {}
        }
    }
    cpio_trailer(&mut out, &mut ino);

    Ok((out, spec))
}

fn apply_layer(layer: &[u8], files: &mut BTreeMap<String, Entry>) -> Result<(), OciError> {
    let mut archive = tar::Archive::new(layer);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry
            .path()?
            .to_str()
            .ok_or_else(|| OciError::Tar("non-utf8 path".into()))?
            .trim_start_matches("./")
            .to_string();
        let path = format!("/{path}");
        let base = path.rsplit('/').next().unwrap_or("");

        // Whiteouts.
        if let Some(name) = base.strip_prefix(".wh..wh..opq") {
            let _ = name;
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
                files.insert(path, Entry::File(data, mode));
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
            _ => {} // Devices, fifos, hardlinks: skipped in v1.
        }
    }
    Ok(())
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn cpio_header(
    out: &mut Vec<u8>,
    ino: u64,
    name: &str,
    mode: u32,
    filesize: u64,
) {
    let namesize = (name.len() + 1) as u64;
    let header = format!(
        "070701{ino:08x}{mode:08x}{uid:08x}{gid:08x}{nlink:08x}{mtime:08x}{filesize:08x}{devmajor:08x}{devminor:08x}{rdevmajor:08x}{rdevminor:08x}{namesize:08x}{check:08x}",
        ino = ino,
        mode = mode,
        uid = 1,
        gid = 1,
        nlink = 1,
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
    use std::io::Write;

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *data)
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn test_image() -> Vec<u8> {
        let layer1 = tar_bytes(&[("app/hello.txt", b"hello")]);
        let layer2 = tar_bytes(&[
            ("app/.wh.hello.txt", b""),
            ("bin/tool", b"\x7fELF-tool"),
        ]);
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
    fn test_cpio_is_parseable() {
        let (cpio, _) = flatten(&test_image()).unwrap();
        assert!(cpio.starts_with(b"070701"));
        assert!(cpio.windows(10).any(|w| w == b"TRAILER!!!"));
        assert_eq!(cpio.len() % 512, 0);
    }

    /// Full image→VM path: build a tiny image containing a static payload
    /// binary, flatten it to an initramfs, boot it, and read the payload's
    /// output on the serial console. Requires KVM.
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn test_boot_container_image() {
        use std::io::Write;
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
            image_builder.append_data(&mut header, name, &data[..]).unwrap();
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

        let resources: vmm::resources::VmResources = vmm::test_utils::mock_resources::MockVmResources::new()
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
