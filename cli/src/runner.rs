// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! One-timeline execution and replay bundles.
//!
//! A bundle is deliberately self-contained. It contains copies of the three
//! executable inputs, the source plan, a plan whose artifact paths are local
//! to the bundle, logs, and the final result. Replaying never reads the test
//! directory that created the bundle.

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{load_plan, LoadError, RunPlan};

const READY_MARKER: &[u8] = b"THES:M:42";
const API_READY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub enum RunError {
    Manifest(LoadError),
    BundleExists(PathBuf),
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    Copy {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
    ParsePlan {
        path: PathBuf,
        source: serde_json::Error,
    },
    Serialize(serde_json::Error),
    InvalidBundle {
        path: PathBuf,
        reason: String,
    },
    DigestMismatch {
        path: PathBuf,
    },
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    Api {
        endpoint: &'static str,
        reason: String,
    },
    MissingStdin,
    GuestNeverReady,
    TimedOut {
        seconds: u64,
    },
    GuestExited {
        status: String,
    },
    UnsupportedNetworkFaults,
}

impl fmt::Display for RunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => error.fmt(formatter),
            Self::BundleExists(path) => write!(
                formatter,
                "replay bundle already exists: {}",
                path.display()
            ),
            Self::Create { path, source } => {
                write!(formatter, "cannot create {}: {source}", path.display())
            }
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "cannot write {}: {source}", path.display())
            }
            Self::Copy { from, to, source } => write!(
                formatter,
                "cannot copy {} to {}: {source}",
                from.display(),
                to.display()
            ),
            Self::ParsePlan { path, source } => {
                write!(formatter, "cannot parse {}: {source}", path.display())
            }
            Self::Serialize(source) => {
                write!(formatter, "cannot serialize replay bundle: {source}")
            }
            Self::InvalidBundle { path, reason } => {
                write!(
                    formatter,
                    "invalid replay bundle {}: {reason}",
                    path.display()
                )
            }
            Self::DigestMismatch { path } => {
                write!(
                    formatter,
                    "artifact digest does not match the replay plan: {}",
                    path.display()
                )
            }
            Self::Spawn { path, source } => {
                write!(
                    formatter,
                    "cannot start Firecracker at {}: {source}",
                    path.display()
                )
            }
            Self::Api { endpoint, reason } => {
                write!(formatter, "Firecracker API {endpoint}: {reason}")
            }
            Self::MissingStdin => {
                write!(formatter, "Firecracker did not provide a serial-input pipe")
            }
            Self::GuestNeverReady => {
                write!(formatter, "guest did not emit the Theseus ready marker")
            }
            Self::TimedOut { seconds } => {
                write!(formatter, "guest did not exit within {seconds} seconds")
            }
            Self::GuestExited { status } => {
                write!(formatter, "guest exited unsuccessfully: {status}")
            }
            Self::UnsupportedNetworkFaults => write!(
                formatter,
                "network drop and partition schedules need a topology; they arrive in P6.4"
            ),
        }
    }
}

impl std::error::Error for RunError {}

#[derive(Debug)]
pub struct TestResult {
    pub bundle: PathBuf,
}

#[derive(Debug)]
pub struct ReplayResult {
    pub logs: PathBuf,
}

#[derive(Debug, Serialize)]
struct ResultRecord<'a> {
    format: &'static str,
    status: &'a str,
    error: Option<String>,
}

/// Execute a manifest once and retain all inputs and outputs in `output`.
pub fn test(manifest: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<TestResult, RunError> {
    let manifest = manifest.as_ref();
    let plan = load_plan(manifest).map_err(RunError::Manifest)?;
    let output = absolute_output(output.as_ref())?;
    let bundle = Bundle::create(&output, manifest, &plan)?;
    let result = execute(&bundle.replay_plan, &bundle.root, &bundle.root);
    bundle.record_result(&result)?;
    result?;
    Ok(TestResult { bundle: output })
}

/// Re-run the exact copied artifacts in a bundle without modifying it.
pub fn replay(bundle: impl AsRef<Path>) -> Result<ReplayResult, RunError> {
    let bundle = fs::canonicalize(bundle.as_ref()).map_err(|source| RunError::Read {
        path: bundle.as_ref().to_path_buf(),
        source,
    })?;
    let plan_path = bundle.join("replay-plan.json");
    let plan = read_plan(&plan_path)?;
    validate_replay_plan(&plan_path, &plan)?;
    let logs = temporary_replay_directory()?;
    execute(&plan, &bundle, &logs)?;
    Ok(ReplayResult { logs })
}

struct Bundle {
    root: PathBuf,
    replay_plan: RunPlan,
}

impl Bundle {
    fn create(root: &Path, manifest: &Path, source_plan: &RunPlan) -> Result<Self, RunError> {
        if root.exists() {
            return Err(RunError::BundleExists(root.to_path_buf()));
        }
        fs::create_dir_all(root).map_err(|source| RunError::Create {
            path: root.to_path_buf(),
            source,
        })?;
        let artifacts = root.join("artifacts");
        fs::create_dir(&artifacts).map_err(|source| RunError::Create {
            path: artifacts.clone(),
            source,
        })?;

        write_json(&root.join("source-plan.json"), source_plan)?;
        fs::copy(manifest, root.join("manifest.toml")).map_err(|source| RunError::Copy {
            from: manifest.to_path_buf(),
            to: root.join("manifest.toml"),
            source,
        })?;

        copy_artifact(
            &source_plan.runtime.firecracker,
            &artifacts.join("firecracker"),
        )?;
        copy_artifact(&source_plan.guest.kernel, &artifacts.join("vmlinux"))?;
        copy_artifact(&source_plan.guest.initramfs, &artifacts.join("initramfs"))?;

        let mut replay_plan = source_plan.clone();
        replay_plan.manifest = "manifest.toml".to_owned();
        replay_plan.runtime.firecracker.path = "artifacts/firecracker".to_owned();
        replay_plan.guest.kernel.path = "artifacts/vmlinux".to_owned();
        replay_plan.guest.initramfs.path = "artifacts/initramfs".to_owned();
        write_json(&root.join("replay-plan.json"), &replay_plan)?;

        Ok(Self {
            root: root.to_path_buf(),
            replay_plan,
        })
    }

    fn record_result(&self, result: &Result<(), RunError>) -> Result<(), RunError> {
        let record = match result {
            Ok(()) => ResultRecord {
                format: "theseus-result-v1",
                status: "passed",
                error: None,
            },
            Err(error) => ResultRecord {
                format: "theseus-result-v1",
                status: "failed",
                error: Some(error.to_string()),
            },
        };
        write_json(&self.root.join("result.json"), &record)
    }
}

fn execute(plan: &RunPlan, artifact_base: &Path, run_directory: &Path) -> Result<(), RunError> {
    if plan.network.drop_ppm != 0 || plan.network.partitioned {
        return Err(RunError::UnsupportedNetworkFaults);
    }
    verify_artifact(artifact_base, &plan.runtime.firecracker)?;
    verify_artifact(artifact_base, &plan.guest.kernel)?;
    verify_artifact(artifact_base, &plan.guest.initramfs)?;

    fs::create_dir_all(run_directory).map_err(|source| RunError::Create {
        path: run_directory.to_path_buf(),
        source,
    })?;
    let socket = run_directory.join("firecracker.sock");
    let serial_log = run_directory.join("serial.log");
    let firecracker_log = run_directory.join("firecracker.log");
    let _ = fs::remove_file(&socket);
    File::create(&serial_log).map_err(|source| RunError::Create {
        path: serial_log.clone(),
        source,
    })?;
    let log = File::create(&firecracker_log).map_err(|source| RunError::Create {
        path: firecracker_log.clone(),
        source,
    })?;

    let firecracker = resolved_path(artifact_base, &plan.runtime.firecracker.path);
    let socket_text = path_text(&socket)?;
    let mut child = Command::new(&firecracker)
        .arg("--api-sock")
        .arg(socket_text)
        .arg("--no-seccomp")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log.try_clone().map_err(|source| {
            RunError::Write {
                path: firecracker_log.clone(),
                source,
            }
        })?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|source| RunError::Spawn {
            path: firecracker.clone(),
            source,
        })?;

    let result = configure_and_wait(&mut child, plan, artifact_base, &socket, &serial_log);
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = fs::remove_file(&socket);
    result
}

fn configure_and_wait(
    child: &mut Child,
    plan: &RunPlan,
    artifact_base: &Path,
    socket: &Path,
    serial_log: &Path,
) -> Result<(), RunError> {
    wait_for_socket(socket, child)?;
    let kernel = path_text(&resolved_path(artifact_base, &plan.guest.kernel.path))?;
    let initramfs = path_text(&resolved_path(artifact_base, &plan.guest.initramfs.path))?;
    let serial_log_text = path_text(serial_log)?;
    api_put(
        socket,
        "/boot-source",
        json!({
            "kernel_image_path": kernel,
            "initrd_path": initramfs,
            "boot_args": "console=ttyS0 reboot=k panic=-1",
        }),
    )?;
    let mut machine = json!({
        "vcpu_count": plan.run.vcpu_count,
        "mem_size_mib": plan.run.mem_size_mib,
    });
    if let Some(virtual_time) = &plan.run.virtual_time {
        machine["virtual_time"] = json!({
            "tick_ns": virtual_time.tick_ns,
            "exits_per_tick": virtual_time.exits_per_tick,
        });
    }
    api_put(socket, "/machine-config", machine)?;
    api_put(
        socket,
        "/serial",
        json!({ "serial_out_path": serial_log_text }),
    )?;
    api_put(socket, "/entropy", json!({ "seed": plan.run.seed }))?;
    api_put(
        socket,
        "/actions",
        json!({ "action_type": "InstanceStart" }),
    )?;

    if !plan.events.is_empty() {
        wait_for_ready(serial_log.to_path_buf(), child, plan.run.timeout_secs)?;
        let stdin = child.stdin.as_mut().ok_or(RunError::MissingStdin)?;
        for event in &plan.events {
            stdin
                .write_all(&decode_hex(&event.data_hex)?)
                .map_err(|source| RunError::Write {
                    path: PathBuf::from("Firecracker serial input"),
                    source,
                })?;
        }
        stdin.flush().map_err(|source| RunError::Write {
            path: PathBuf::from("Firecracker serial input"),
            source,
        })?;
    }
    wait_for_exit(child, plan.run.timeout_secs)
}

fn wait_for_socket(socket: &Path, child: &mut Child) -> Result<(), RunError> {
    let deadline = Instant::now() + API_READY_TIMEOUT;
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|source| RunError::Read {
            path: PathBuf::from("Firecracker process"),
            source,
        })? {
            return Err(RunError::GuestExited {
                status: status.to_string(),
            });
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(RunError::Api {
        endpoint: "startup",
        reason: "API socket did not appear within 5 seconds".to_owned(),
    })
}

fn wait_for_ready(
    serial_log: PathBuf,
    child: &mut Child,
    timeout_secs: u64,
) -> Result<(), RunError> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        let contents = fs::read(&serial_log).map_err(|source| RunError::Read {
            path: serial_log.clone(),
            source,
        })?;
        if contents
            .windows(READY_MARKER.len())
            .any(|window| window == READY_MARKER)
        {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|source| RunError::Read {
            path: PathBuf::from("Firecracker process"),
            source,
        })? {
            return Err(RunError::GuestExited {
                status: status.to_string(),
            });
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(RunError::GuestNeverReady)
}

fn wait_for_exit(child: &mut Child, timeout_secs: u64) -> Result<(), RunError> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(|source| RunError::Read {
            path: PathBuf::from("Firecracker process"),
            source,
        })? {
            return if status.success() {
                Ok(())
            } else {
                Err(RunError::GuestExited {
                    status: status.to_string(),
                })
            };
        }
        thread::sleep(POLL_INTERVAL);
    }
    child.kill().map_err(|source| RunError::Write {
        path: PathBuf::from("Firecracker process"),
        source,
    })?;
    let _ = child.wait();
    Err(RunError::TimedOut {
        seconds: timeout_secs,
    })
}

fn api_put(socket: &Path, endpoint: &'static str, body: Value) -> Result<(), RunError> {
    let mut stream = UnixStream::connect(socket).map_err(|source| RunError::Api {
        endpoint,
        reason: source.to_string(),
    })?;
    stream
        .set_read_timeout(Some(API_READY_TIMEOUT))
        .map_err(|source| RunError::Api {
            endpoint,
            reason: source.to_string(),
        })?;
    stream
        .set_write_timeout(Some(API_READY_TIMEOUT))
        .map_err(|source| RunError::Api {
            endpoint,
            reason: source.to_string(),
        })?;
    let body = serde_json::to_vec(&body).map_err(RunError::Serialize)?;
    let request = format!(
        "PUT {endpoint} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|source| RunError::Api {
            endpoint,
            reason: source.to_string(),
        })?;
    stream.write_all(&body).map_err(|source| RunError::Api {
        endpoint,
        reason: source.to_string(),
    })?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|source| RunError::Api {
            endpoint,
            reason: source.to_string(),
        })?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|source| RunError::Api {
            endpoint,
            reason: source.to_string(),
        })?;
    if !response.starts_with("HTTP/1.1 204") {
        return Err(RunError::Api {
            endpoint,
            reason: response
                .lines()
                .next()
                .unwrap_or("empty response")
                .to_owned(),
        });
    }
    Ok(())
}

fn verify_artifact(base: &Path, artifact: &crate::manifest::ArtifactPlan) -> Result<(), RunError> {
    let path = resolved_path(base, &artifact.path);
    let bytes = fs::read(&path).map_err(|source| RunError::Read {
        path: path.clone(),
        source,
    })?;
    if hex(Sha256::digest(bytes)) != artifact.sha256 {
        return Err(RunError::DigestMismatch { path });
    }
    Ok(())
}

fn copy_artifact(
    artifact: &crate::manifest::ArtifactPlan,
    destination: &Path,
) -> Result<(), RunError> {
    let from = PathBuf::from(&artifact.path);
    fs::copy(&from, destination).map_err(|source| RunError::Copy {
        from,
        to: destination.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), RunError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(RunError::Serialize)?;
    fs::write(path, bytes).map_err(|source| RunError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn read_plan(path: &Path) -> Result<RunPlan, RunError> {
    let input = fs::read(path).map_err(|source| RunError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&input).map_err(|source| RunError::ParsePlan {
        path: path.to_path_buf(),
        source,
    })
}

fn validate_replay_plan(path: &Path, plan: &RunPlan) -> Result<(), RunError> {
    if plan.format != "theseus-run-plan-v1" {
        return Err(RunError::InvalidBundle {
            path: path.to_path_buf(),
            reason: format!("unsupported plan format {}", plan.format),
        });
    }
    for artifact in [
        &plan.runtime.firecracker,
        &plan.guest.kernel,
        &plan.guest.initramfs,
    ] {
        if Path::new(&artifact.path).is_absolute() || artifact.path.contains("..") {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "artifact path must remain inside the replay bundle".to_owned(),
            });
        }
    }
    if plan.run.vcpu_count == 0 || plan.run.mem_size_mib == 0 || plan.run.timeout_secs == 0 {
        return Err(RunError::InvalidBundle {
            path: path.to_path_buf(),
            reason: "runner settings must be greater than zero".to_owned(),
        });
    }
    if plan.network.drop_ppm > 1_000_000 {
        return Err(RunError::InvalidBundle {
            path: path.to_path_buf(),
            reason: "network.drop_ppm must be at most 1000000".to_owned(),
        });
    }
    if let Some(virtual_time) = &plan.run.virtual_time {
        if virtual_time.tick_ns == 0 || virtual_time.exits_per_tick == 0 {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "virtual-time settings must be greater than zero".to_owned(),
            });
        }
    }
    for event in &plan.events {
        if decode_hex(&event.data_hex).is_err() {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "event data must be non-empty, even-length hexadecimal".to_owned(),
            });
        }
    }
    Ok(())
}

fn temporary_replay_directory() -> Result<PathBuf, RunError> {
    let base = std::env::temp_dir().join(format!("theseus-replay-{}", std::process::id()));
    for attempt in 0..100 {
        let directory = if attempt == 0 {
            base.clone()
        } else {
            PathBuf::from(format!("{}-{attempt}", base.display()))
        };
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(RunError::Create {
                    path: directory,
                    source,
                });
            }
        }
    }
    Err(RunError::BundleExists(base))
}

fn resolved_path(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn absolute_output(path: &Path) -> Result<PathBuf, RunError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|source| RunError::Create {
            path: path.to_path_buf(),
            source,
        })
}

fn path_text(path: &Path) -> Result<String, RunError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| RunError::InvalidBundle {
            path: path.to_path_buf(),
            reason: "path is not valid UTF-8".to_owned(),
        })
}

fn decode_hex(value: &str) -> Result<Vec<u8>, RunError> {
    if value.is_empty() || !value.len().is_multiple_of(2) {
        return Err(RunError::InvalidBundle {
            path: PathBuf::from("replay-plan.json"),
            reason: "event data must be non-empty, even-length hexadecimal".to_owned(),
        });
    }
    (0..value.len())
        .step_by(2)
        .map(|offset| {
            u8::from_str_radix(&value[offset..offset + 2], 16).map_err(|_| {
                RunError::InvalidBundle {
                    path: PathBuf::from("replay-plan.json"),
                    reason: format!("invalid event byte at offset {offset}"),
                }
            })
        })
        .collect()
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_plan;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("runtime")).unwrap();
        fs::create_dir_all(directory.path().join("guest")).unwrap();
        fs::write(directory.path().join("runtime/firecracker"), b"firecracker").unwrap();
        fs::set_permissions(
            directory.path().join("runtime/firecracker"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(directory.path().join("guest/vmlinux"), b"kernel").unwrap();
        fs::write(directory.path().join("guest/initramfs"), b"initramfs").unwrap();
        fs::write(
            directory.path().join("theseus.toml"),
            r#"version = 1
[runtime]
firecracker = "runtime/firecracker"
[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs"
[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128
"#,
        )
        .unwrap();
        directory
    }

    #[test]
    fn bundle_copies_artifacts_and_rewrites_the_plan_to_local_paths() {
        let directory = fixture();
        let manifest = directory.path().join("theseus.toml");
        let plan = load_plan(&manifest).unwrap();
        let output = directory.path().join("replay");
        let bundle = Bundle::create(&output, &manifest, &plan).unwrap();

        assert!(output.join("source-plan.json").is_file());
        assert!(output.join("manifest.toml").is_file());
        assert_eq!(
            bundle.replay_plan.runtime.firecracker.path,
            "artifacts/firecracker"
        );
        assert_eq!(
            fs::read(output.join("artifacts/firecracker")).unwrap(),
            b"firecracker"
        );
        validate_replay_plan(&output.join("replay-plan.json"), &bundle.replay_plan).unwrap();
    }

    #[test]
    fn replay_rejects_an_artifact_path_outside_the_bundle() {
        let directory = fixture();
        let manifest = directory.path().join("theseus.toml");
        let mut plan = load_plan(&manifest).unwrap();
        plan.runtime.firecracker.path = "../firecracker".to_owned();
        let error =
            validate_replay_plan(&directory.path().join("replay-plan.json"), &plan).unwrap_err();
        assert!(error.to_string().contains("must remain inside"));
    }

    #[test]
    fn a_failed_launch_still_leaves_an_inspectable_bundle() {
        let directory = fixture();
        let output = directory.path().join("replay");
        let _error = test(directory.path().join("theseus.toml"), &output).unwrap_err();
        let result: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["status"], "failed");
        assert!(output.join("serial.log").is_file());
        assert!(output.join("firecracker.log").is_file());
    }

    #[test]
    fn api_client_sends_a_complete_put_request() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("firecracker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            stream.read_to_string(&mut request).unwrap();
            assert!(request.starts_with("PUT /entropy HTTP/1.1"));
            assert!(request.contains("{\"seed\":42}"));
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        api_put(&socket, "/entropy", json!({ "seed": 42 })).unwrap();
        server.join().unwrap();
    }
}
