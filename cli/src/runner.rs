// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! One-timeline execution and replay bundles.
//!
//! A bundle is deliberately self-contained. It contains copies of every
//! executable and guest input, the source plan, a plan whose artifact paths are local
//! to the bundle, logs, and the final result. Replaying never reads the test
//! directory that created the bundle.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{load_plan, CheckKind, LoadError, RunPlan};

const READY_MARKER: &[u8] = b"THES:M:42";
const API_READY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const EXECUTION_REPLAY_FORMAT: &str = "theseus-replay-plan-v2";

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
    ImageAdapter {
        path: PathBuf,
        reason: String,
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
    UnsupportedStorageFaults,
    ChecksFailed {
        names: Vec<String>,
    },
    ReplayFailed {
        logs: PathBuf,
        reason: String,
    },
}

impl fmt::Display for RunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReplayFailed { logs, reason } => write!(formatter, "replay failed: {reason}; evidence: {}", logs.display()),
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
            Self::ImageAdapter { path, reason } => {
                write!(formatter, "container image adapter {}: {reason}", path.display())
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
                "simulated network settings need a topology; use `theseus compose test`"
            ),
            Self::UnsupportedStorageFaults => write!(
                formatter,
                "simulated storage faults require `theseus compose test` on a published Linux runtime"
            ),
            Self::ChecksFailed { names } => {
                write!(formatter, "checks failed: {}", names.join(", "))
            }
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

#[derive(Debug, Clone, Serialize)]
struct CheckResult {
    name: String,
    kind: String,
    status: &'static str,
    detail: String,
}

#[derive(Debug)]
struct Execution {
    checks: Vec<CheckResult>,
}

impl Execution {
    fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.status == "passed")
    }

    fn failed_names(&self) -> Vec<String> {
        self.checks
            .iter()
            .filter(|check| check.status == "failed")
            .map(|check| check.name.clone())
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct ResultRecord {
    format: &'static str,
    status: &'static str,
    error: Option<String>,
    checks: Vec<CheckResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_evidence: Option<&'static str>,
}

/// Execute a manifest once and retain all inputs and outputs in `output`.
pub fn test(manifest: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<TestResult, RunError> {
    let manifest = manifest.as_ref();
    let plan = load_plan(manifest).map_err(RunError::Manifest)?;
    if !plan.storage.is_empty() {
        return Err(RunError::UnsupportedStorageFaults);
    }
    let output = absolute_output(output.as_ref())?;
    let bundle = Bundle::create(&output, manifest, &plan)?;
    let execution = match execute(&bundle.replay_plan, &bundle.root, &bundle.root, None) {
        Ok(execution) => execution,
        Err(error) => {
            bundle.record_error(&error)?;
            return Err(error);
        }
    };
    bundle.record_execution(&execution)?;
    if !execution.passed() {
        return Err(RunError::ChecksFailed {
            names: execution.failed_names(),
        });
    }
    Ok(TestResult { bundle: output })
}

/// Re-run the exact copied artifacts in a bundle without modifying it.
pub fn replay(bundle: impl AsRef<Path>) -> Result<ReplayResult, RunError> {
    replay_inner(bundle.as_ref(), None)
}

/// Replay into a new, user-selected diagnostics directory without modifying the bundle.
pub fn replay_to(
    bundle: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<ReplayResult, RunError> {
    replay_inner(bundle.as_ref(), Some(output.as_ref()))
}

fn replay_inner(bundle: &Path, output: Option<&Path>) -> Result<ReplayResult, RunError> {
    let bundle = fs::canonicalize(bundle).map_err(|source| RunError::Read {
        path: bundle.to_path_buf(),
        source,
    })?;
    let plan_path = bundle.join("replay-plan.json");
    let plan = read_plan(&plan_path)?;
    validate_replay_plan(&plan_path, &plan)?;
    let expected = if plan.format == EXECUTION_REPLAY_FORMAT {
        let path = bundle.join("execution.json");
        let evidence = crate::execution::Evidence::read(&path, plan.run.vcpu_count)?;
        evidence.require_replayable(&path)?;
        Some(evidence)
    } else {
        None
    };
    let logs = match output {
        Some(path) => {
            let path = absolute_output(path)?;
            if path.exists() {
                return Err(RunError::BundleExists(path));
            }
            fs::create_dir_all(&path).map_err(|source| RunError::Create {
                path: path.clone(),
                source,
            })?;
            path
        }
        None => temporary_replay_directory()?,
    };
    let output = Bundle {
        root: logs.clone(),
        replay_plan: plan.clone(),
    };
    let execution = match execute(&plan, &bundle, &logs, expected.as_ref()) {
        Ok(execution) => execution,
        Err(error) => {
            output.record_error(&error)?;
            return Err(RunError::ReplayFailed {
                logs,
                reason: error.to_string(),
            });
        }
    };
    output.record_execution(&execution)?;
    if !execution.passed() {
        return Err(RunError::ReplayFailed {
            logs,
            reason: execution
                .checks
                .iter()
                .filter(|check| check.status == "failed")
                .map(|check| format!("{}: {}", check.name, check.detail))
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
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
        if let Some(initramfs) = &source_plan.guest.initramfs {
            copy_artifact(initramfs, &artifacts.join("initramfs"))?;
        }
        if let Some(image) = &source_plan.guest.image {
            copy_artifact(image, &artifacts.join("image.tar"))?;
        }
        if let Some(adapter) = &source_plan.runtime.image_adapter {
            copy_artifact(adapter, &artifacts.join("theseus-image"))?;
        }

        let mut replay_plan = source_plan.clone();
        // A versioned plan makes execution.json mandatory. Removing that file
        // or result.json cannot silently select legacy seed-only replay.
        replay_plan.format = EXECUTION_REPLAY_FORMAT.to_owned();
        replay_plan.manifest = "manifest.toml".to_owned();
        replay_plan.runtime.firecracker.path = "artifacts/firecracker".to_owned();
        if let Some(adapter) = &mut replay_plan.runtime.image_adapter {
            adapter.path = "artifacts/theseus-image".to_owned();
        }
        replay_plan.guest.kernel.path = "artifacts/vmlinux".to_owned();
        if let Some(initramfs) = &mut replay_plan.guest.initramfs {
            initramfs.path = "artifacts/initramfs".to_owned();
        }
        if let Some(image) = &mut replay_plan.guest.image {
            image.path = "artifacts/image.tar".to_owned();
        }
        write_json(&root.join("replay-plan.json"), &replay_plan)?;

        Ok(Self {
            root: root.to_path_buf(),
            replay_plan,
        })
    }

    fn record_execution(&self, execution: &Execution) -> Result<(), RunError> {
        let record = ResultRecord {
            format: "theseus-result-v1",
            status: if execution.passed() {
                "passed"
            } else {
                "failed"
            },
            error: None,
            checks: execution.checks.clone(),
            execution_evidence: (self.replay_plan.format == EXECUTION_REPLAY_FORMAT)
                .then_some("execution.json"),
        };
        write_json(&self.root.join("result.json"), &record)
    }

    fn record_error(&self, error: &RunError) -> Result<(), RunError> {
        let record = ResultRecord {
            format: "theseus-result-v1",
            status: "failed",
            error: Some(error.to_string()),
            checks: Vec::new(),
            execution_evidence: (self.replay_plan.format == EXECUTION_REPLAY_FORMAT)
                .then_some("execution.json"),
        };
        write_json(&self.root.join("result.json"), &record)
    }
}

fn execute(
    plan: &RunPlan,
    artifact_base: &Path,
    run_directory: &Path,
    expected: Option<&crate::execution::Evidence>,
) -> Result<Execution, RunError> {
    if plan.network.loopback
        || plan.network.drop_ppm != 0
        || plan.network.duplicate_ppm != 0
        || plan.network.corrupt_ppm != 0
        || plan.network.partitioned
        || plan.network.latency_rounds != 0
        || plan.network.jitter_rounds != 0
        || plan.network.tx_bytes_per_round != 0
        || plan.network.mtu_bytes != 0
        || plan.network.tx_queue_frames != 0
        || plan.network.rx_queue_frames != 0
    {
        return Err(RunError::UnsupportedNetworkFaults);
    }
    verify_artifact(artifact_base, &plan.runtime.firecracker)?;
    verify_artifact(artifact_base, &plan.guest.kernel)?;

    fs::create_dir_all(run_directory).map_err(|source| RunError::Create {
        path: run_directory.to_path_buf(),
        source,
    })?;
    let initramfs = materialize_initramfs(plan, artifact_base, run_directory)?;
    let capture = plan.format == EXECUTION_REPLAY_FORMAT;
    let expected_trace = expected
        .map(|evidence| {
            let path = run_directory.join("expected-execution.json");
            write_json(&path, &evidence.machine_execution_trace)?;
            Ok::<_, RunError>(path)
        })
        .transpose()?;
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

    let result = configure_and_wait(
        &mut child,
        plan,
        artifact_base,
        &initramfs,
        &socket,
        &serial_log,
        capture,
        expected_trace.as_deref(),
    );
    if result.is_err() {
        // Retain a stable diagnostic cut when boot succeeded but input or
        // readiness failed. Failure to capture is never a successful replay.
        if capture && child.try_wait().ok().flatten().is_none() {
            let _ = pause_and_flush(&socket);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = fs::remove_file(&socket);
    let mut execution = result?;
    if capture {
        let evidence = crate::execution::Evidence::read(
            &run_directory.join("execution.json"),
            plan.run.vcpu_count,
        )?;
        execution.checks.push(match &evidence.replay_error {
            Some(error) => failed("machine_execution", "machine_execution", error),
            None => passed(
                "machine_execution",
                "machine_execution",
                "complete machine stream and local ledgers retained",
            ),
        });
        if let Some(expected) = expected {
            let matches = expected == &evidence;
            execution.checks.push(if matches {
                passed(
                    "replay_machine_execution",
                    "machine_execution",
                    "the exact ordered stream actively governed replay through guest exit",
                )
            } else {
                failed(
                    "replay_machine_execution",
                    "machine_execution",
                    evidence
                        .replay_error
                        .as_deref()
                        .unwrap_or("machine stream, local ledger, or terminal boundary changed"),
                )
            });
        }
    }
    Ok(execution)
}

fn materialize_initramfs(
    plan: &RunPlan,
    artifact_base: &Path,
    run_directory: &Path,
) -> Result<PathBuf, RunError> {
    if let Some(initramfs) = &plan.guest.initramfs {
        verify_artifact(artifact_base, initramfs)?;
        return Ok(resolved_path(artifact_base, &initramfs.path));
    }

    let image = plan
        .guest
        .image
        .as_ref()
        .ok_or_else(|| RunError::InvalidBundle {
            path: run_directory.to_path_buf(),
            reason: "guest must contain an initramfs or image".to_owned(),
        })?;
    let adapter = plan
        .runtime
        .image_adapter
        .as_ref()
        .ok_or_else(|| RunError::InvalidBundle {
            path: run_directory.to_path_buf(),
            reason: "guest.image requires a locked runtime.image_adapter".to_owned(),
        })?;
    verify_artifact(artifact_base, image)?;
    verify_artifact(artifact_base, adapter)?;
    let image = resolved_path(artifact_base, &image.path);
    let adapter = resolved_path(artifact_base, &adapter.path);
    let output = run_directory.join("container-image-initramfs.cpio");
    let mut command = Command::new(&adapter);
    command
        .arg("flatten")
        .arg(&image)
        .arg("--output")
        .arg(&output);
    if let Some(service) = &plan.container_service {
        let contract = run_directory.join("container-service.json");
        write_json(&contract, service)?;
        command.arg("--service").arg(contract);
    }
    let status = command.status().map_err(|source| RunError::ImageAdapter {
        path: adapter.clone(),
        reason: source.to_string(),
    })?;
    if !status.success() {
        return Err(RunError::ImageAdapter {
            path: adapter,
            reason: format!("exited with {status}"),
        });
    }
    if !output.is_file() {
        return Err(RunError::ImageAdapter {
            path: adapter,
            reason: format!("did not create {}", output.display()),
        });
    }
    Ok(output)
}

fn configure_and_wait(
    child: &mut Child,
    plan: &RunPlan,
    artifact_base: &Path,
    initramfs: &Path,
    socket: &Path,
    serial_log: &Path,
    capture: bool,
    expected_trace: Option<&Path>,
) -> Result<Execution, RunError> {
    wait_for_socket(socket, child)?;
    let kernel = path_text(&resolved_path(artifact_base, &plan.guest.kernel.path))?;
    let initramfs = path_text(initramfs)?;
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
    if capture {
        api_put(
            socket,
            "/execution",
            json!({
                "evidence_path": path_text(&serial_log.with_file_name("execution.json"))?,
                "replay_trace_path": expected_trace.map(path_text).transpose()?,
            }),
        )?;
    }
    api_put(
        socket,
        "/actions",
        json!({ "action_type": "InstanceStart" }),
    )?;

    if !plan.events.is_empty() {
        wait_for_ready(serial_log.to_path_buf(), child, plan.run.timeout_secs)?;
        for event in &plan.events {
            if capture {
                // Keep each manifest payload one exact host-input decision.
                api_put(
                    socket,
                    "/serial-input",
                    json!({ "data_hex": event.data_hex.to_ascii_lowercase() }),
                )?;
            } else {
                child
                    .stdin
                    .as_mut()
                    .ok_or(RunError::MissingStdin)?
                    .write_all(&decode_hex(&event.data_hex)?)
                    .map_err(|source| RunError::Write {
                        path: PathBuf::from("Firecracker serial input"),
                        source,
                    })?;
            }
        }
        if !capture {
            child
                .stdin
                .as_mut()
                .ok_or(RunError::MissingStdin)?
                .flush()
                .map_err(|source| RunError::Write {
                    path: PathBuf::from("Firecracker serial input"),
                    source,
                })?;
        }
    }
    let terminal = wait_for_exit(child, plan.run.timeout_secs, capture.then_some(socket))?;
    evaluate_checks(plan, serial_log, terminal)
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

enum Terminal {
    Exited(std::process::ExitStatus),
    TimedOut,
}

fn pause_and_flush(socket: &Path) -> Result<(), RunError> {
    api_request(socket, "PATCH", "/vm", json!({ "state": "Paused" }))?;
    api_request(socket, "PATCH", "/execution", json!({}))
}

fn wait_for_exit(
    child: &mut Child,
    timeout_secs: u64,
    capture_socket: Option<&Path>,
) -> Result<Terminal, RunError> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(|source| RunError::Read {
            path: PathBuf::from("Firecracker process"),
            source,
        })? {
            return Ok(Terminal::Exited(status));
        }
        thread::sleep(POLL_INTERVAL);
    }
    if let Some(socket) = capture_socket {
        if let Err(error) = pause_and_flush(socket) {
            // A guest can terminate while its timeout pause is in flight.
            if let Some(status) = child.try_wait().map_err(|source| RunError::Read {
                path: PathBuf::from("Firecracker process"),
                source,
            })? {
                return Ok(Terminal::Exited(status));
            }
            return Err(error);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(Terminal::TimedOut)
}

fn evaluate_checks(
    plan: &RunPlan,
    serial_log: &Path,
    terminal: Terminal,
) -> Result<Execution, RunError> {
    let mut checks = Vec::new();
    match terminal {
        Terminal::Exited(status) if status.success() => {
            checks.push(passed(
                "guest_exit",
                "guest_exit",
                "guest exited with status 0",
            ));
            checks.push(passed(
                "completion",
                "completion",
                "guest exited before the configured timeout",
            ));
        }
        Terminal::Exited(status) => {
            checks.push(failed(
                "guest_exit",
                "guest_exit",
                format!("guest exited with {status}"),
            ));
            checks.push(passed(
                "completion",
                "completion",
                "guest exited before the configured timeout",
            ));
        }
        Terminal::TimedOut => {
            checks.push(failed(
                "guest_exit",
                "guest_exit",
                "guest was killed after the configured timeout",
            ));
            checks.push(failed(
                "completion",
                "completion",
                format!(
                    "guest did not exit within {} seconds",
                    plan.run.timeout_secs
                ),
            ));
        }
    }

    let serial = fs::read(serial_log).map_err(|source| RunError::Read {
        path: serial_log.to_path_buf(),
        source,
    })?;
    if let Some(service) = &plan.container_service {
        if service.ready.is_some() {
            let ready = b"THES:HTTP:ready:PASS";
            let found = contains(&serial, ready);
            checks.push(if found {
                passed(
                    "container_service.ready",
                    "http_ready",
                    "service reported HTTP readiness",
                )
            } else {
                failed(
                    "container_service.ready",
                    "http_ready",
                    "service did not report HTTP readiness",
                )
            });
        }
        for assertion in &service.assertions {
            let expected = format!("THES:HTTP:{}:PASS", assertion.name);
            let found = contains(&serial, expected.as_bytes());
            let name = format!("container_service.{}", assertion.name);
            checks.push(if found {
                passed(
                    &name,
                    "http_assertion",
                    format!("HTTP assertion {:?} passed", assertion.name),
                )
            } else {
                failed(
                    &name,
                    "http_assertion",
                    format!("HTTP assertion {:?} did not pass", assertion.name),
                )
            });
        }
        for operation in &service.operations {
            let expected = format!("THES:HTTP:operation:{}:PASS", operation.name);
            let found = contains(&serial, expected.as_bytes());
            let name = format!("container_service.operation.{}", operation.name);
            checks.push(if found {
                passed(
                    &name,
                    "http_operation",
                    format!("HTTP operation {:?} passed", operation.name),
                )
            } else {
                failed(
                    &name,
                    "http_operation",
                    format!("HTTP operation {:?} did not pass", operation.name),
                )
            });
        }
        if service.grpc_ready.is_some() {
            let ready = b"THES:GRPC:ready:PASS";
            let found = contains(&serial, ready);
            checks.push(if found {
                passed(
                    "container_service.grpc_ready",
                    "grpc_ready",
                    "service reported gRPC readiness",
                )
            } else {
                failed(
                    "container_service.grpc_ready",
                    "grpc_ready",
                    "service did not report gRPC readiness",
                )
            });
        }
        for assertion in &service.grpc_assertions {
            let expected = format!("THES:GRPC:{}:PASS", assertion.name);
            let found = contains(&serial, expected.as_bytes());
            let name = format!("container_service.{}", assertion.name);
            checks.push(if found {
                passed(
                    &name,
                    "grpc_assertion",
                    format!("gRPC assertion {:?} passed", assertion.name),
                )
            } else {
                failed(
                    &name,
                    "grpc_assertion",
                    format!("gRPC assertion {:?} did not pass", assertion.name),
                )
            });
        }
        for operation in &service.grpc_operations {
            let expected = format!("THES:GRPC:operation:{}:PASS", operation.name);
            let found = contains(&serial, expected.as_bytes());
            let name = format!("container_service.operation.{}", operation.name);
            checks.push(if found {
                passed(
                    &name,
                    "grpc_operation",
                    format!("gRPC health operation {:?} passed", operation.name),
                )
            } else {
                failed(
                    &name,
                    "grpc_operation",
                    format!("gRPC health operation {:?} did not pass", operation.name),
                )
            });
        }
        for operation in &service.shell_operations {
            let expected = format!("THES:SHELL:operation:{}:PASS", operation.name);
            let found = contains(&serial, expected.as_bytes());
            let name = format!("container_service.operation.{}", operation.name);
            checks.push(if found {
                passed(
                    &name,
                    "shell_operation",
                    format!("shell operation {:?} passed", operation.name),
                )
            } else {
                failed(
                    &name,
                    "shell_operation",
                    format!("shell operation {:?} did not pass", operation.name),
                )
            });
        }
    }
    for check in &plan.checks {
        let (kind, expected, found) = match &check.kind {
            CheckKind::SerialContains => (
                "serial_contains",
                check.value.as_bytes().to_vec(),
                contains(&serial, check.value.as_bytes()),
            ),
            CheckKind::SerialNotContains => (
                "serial_not_contains",
                check.value.as_bytes().to_vec(),
                !contains(&serial, check.value.as_bytes()),
            ),
            CheckKind::MarkerSeen => {
                let marker = format!("THES:M:{}", check.value);
                let found = contains(&serial, marker.as_bytes());
                ("marker_seen", marker.into_bytes(), found)
            }
            CheckKind::MarkerNotSeen => {
                let marker = format!("THES:M:{}", check.value);
                let found = !contains(&serial, marker.as_bytes());
                ("marker_not_seen", marker.into_bytes(), found)
            }
        };
        let display = String::from_utf8_lossy(&expected);
        if found {
            checks.push(passed(
                &check.name,
                kind,
                format!("serial log satisfied {kind} for {display:?}"),
            ));
        } else {
            checks.push(failed(
                &check.name,
                kind,
                format!("serial log did not satisfy {kind} for {display:?}"),
            ));
        }
    }
    Ok(Execution { checks })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn passed(name: &str, kind: &str, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        name: name.to_owned(),
        kind: kind.to_owned(),
        status: "passed",
        detail: detail.into(),
    }
}

fn failed(name: &str, kind: &str, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        name: name.to_owned(),
        kind: kind.to_owned(),
        status: "failed",
        detail: detail.into(),
    }
}

fn api_put(socket: &Path, endpoint: &'static str, body: Value) -> Result<(), RunError> {
    api_request(socket, "PUT", endpoint, body)
}

fn api_request(
    socket: &Path,
    method: &str,
    endpoint: &'static str,
    body: Value,
) -> Result<(), RunError> {
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
        "{method} {endpoint} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
    // Firecracker keeps HTTP connections open. Frame the response instead of
    // half-closing the request or waiting for EOF from the server.
    let response = read_api_response(stream).map_err(|source| RunError::Api {
        endpoint,
        reason: source.to_string(),
    })?;
    if response.split_whitespace().nth(1) != Some("204") {
        return Err(RunError::Api {
            endpoint,
            reason: if response.is_empty() {
                "empty response".into()
            } else {
                response
            },
        });
    }
    Ok(())
}

fn read_api_response(stream: UnixStream) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind};
    let invalid = |message| Error::new(ErrorKind::InvalidData, message);
    let mut stream = BufReader::new(stream);
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() == 16 * 1024 {
            return Err(invalid("API response headers exceed 16 KiB"));
        }
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    let header = String::from_utf8(header).map_err(|_| invalid("invalid API response headers"))?;
    let mut lines = header.split("\r\n");
    let status = lines.next().unwrap_or_default();
    let mut fields = status.split_whitespace();
    if fields.next() != Some("HTTP/1.1")
        || !fields
            .next()
            .is_some_and(|code| code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid("invalid API response status"));
    }
    let mut length = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("invalid API response header"))?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid("unsupported API response transfer encoding"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            let value = value
                .trim()
                .parse::<usize>()
                .map_err(|_| invalid("invalid API response length"))?;
            if length.replace(value).is_some() || value > 128 * 1024 {
                return Err(invalid("duplicate or oversized API response length"));
            }
        }
    }
    let length = match length {
        Some(length) => length,
        None if status.split_whitespace().nth(1) == Some("204") => 0,
        None => return Err(invalid("missing API response length")),
    };
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok(header + &String::from_utf8_lossy(&body))
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
    if plan.format != "theseus-run-plan-v1" && plan.format != EXECUTION_REPLAY_FORMAT {
        return Err(RunError::InvalidBundle {
            path: path.to_path_buf(),
            reason: format!("unsupported plan format {}", plan.format),
        });
    }
    let mut artifacts = vec![&plan.runtime.firecracker, &plan.guest.kernel];
    if let Some(initramfs) = &plan.guest.initramfs {
        artifacts.push(initramfs);
    }
    if let Some(image) = &plan.guest.image {
        artifacts.push(image);
    }
    if let Some(adapter) = &plan.runtime.image_adapter {
        artifacts.push(adapter);
    }
    for artifact in artifacts {
        if Path::new(&artifact.path).is_absolute() || artifact.path.contains("..") {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "artifact path must remain inside the replay bundle".to_owned(),
            });
        }
    }
    if plan.guest.initramfs.is_some() == plan.guest.image.is_some() {
        return Err(RunError::InvalidBundle {
            path: path.to_path_buf(),
            reason: "guest must contain exactly one of initramfs or image".to_owned(),
        });
    }
    if plan.guest.image.is_some() != plan.runtime.image_adapter.is_some() {
        return Err(RunError::InvalidBundle {
            path: path.to_path_buf(),
            reason: "guest.image and runtime.image_adapter must be locked together".to_owned(),
        });
    }
    if let Some(service) = &plan.container_service {
        if plan.guest.image.is_none() {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "container_service requires guest.image".to_owned(),
            });
        }
        if service.ready.is_none() && service.grpc_ready.is_none() {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "container_service needs ready or grpc_ready".to_owned(),
            });
        }
        if service
            .ready
            .as_ref()
            .is_some_and(|ready| ready.attempts == 0 || ready.interval_millis == 0)
            || service
                .grpc_ready
                .as_ref()
                .is_some_and(|ready| ready.attempts == 0 || ready.interval_millis == 0)
        {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "container_service readiness settings must be greater than zero".to_owned(),
            });
        }
        let mut assertion_names = HashSet::new();
        for assertion in &service.assertions {
            if assertion.name == "ready"
                || !assertion
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                || !assertion_names.insert(&assertion.name)
            {
                return Err(RunError::InvalidBundle {
                    path: path.to_path_buf(),
                    reason: "container_service assertion names must be unique and safe".to_owned(),
                });
            }
        }
        for operation in &service.operations {
            if operation.name == "ready"
                || !operation
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                || !assertion_names.insert(&operation.name)
            {
                return Err(RunError::InvalidBundle {
                    path: path.to_path_buf(),
                    reason: "container_service operation names must be unique and safe".to_owned(),
                });
            }
            if !(100..=599).contains(&operation.expect_status)
                || operation.body_contains.as_deref() == Some("")
            {
                return Err(RunError::InvalidBundle {
                    path: path.to_path_buf(),
                    reason: "container_service operation contract is invalid".to_owned(),
                });
            }
        }
        for assertion in &service.grpc_assertions {
            if assertion.name == "ready"
                || !assertion
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                || !assertion_names.insert(&assertion.name)
            {
                return Err(RunError::InvalidBundle {
                    path: path.to_path_buf(),
                    reason: "container_service assertion names must be unique and safe".to_owned(),
                });
            }
        }
        for operation in &service.grpc_operations {
            if operation.name == "ready"
                || !operation
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                || !assertion_names.insert(&operation.name)
            {
                return Err(RunError::InvalidBundle {
                    path: path.to_path_buf(),
                    reason: "container_service operation names must be unique and safe".to_owned(),
                });
            }
        }
        for operation in &service.shell_operations {
            if operation.name == "ready"
                || !operation
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                || !assertion_names.insert(&operation.name)
            {
                return Err(RunError::InvalidBundle {
                    path: path.to_path_buf(),
                    reason: "container_service operation names must be unique and safe".to_owned(),
                });
            }
            if operation.command.is_empty()
                || !operation.command[0].starts_with('/')
                || operation
                    .command
                    .iter()
                    .any(|argument| argument.is_empty() || argument.contains('\0'))
                || operation.output_contains.as_deref() == Some("")
                || operation.environment.iter().any(|(key, value)| {
                    key.is_empty()
                        || key.contains('=')
                        || key.contains('\0')
                        || value.contains('\0')
                        || key == "THESEUS_CHANNEL"
                })
            {
                return Err(RunError::InvalidBundle {
                    path: path.to_path_buf(),
                    reason: "container_service shell operation contract is invalid".to_owned(),
                });
            }
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
    let mut check_names = HashSet::new();
    for check in &plan.checks {
        if check.name.trim().is_empty()
            || check.name == "guest_exit"
            || check.name == "completion"
            || check.name == "machine_execution"
            || check.name == "replay_machine_execution"
            || !check_names.insert(&check.name)
            || check.value.is_empty()
        {
            return Err(RunError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "checks must use unique non-reserved names and non-empty values".to_owned(),
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
    if value.is_empty()
        || !value.len().is_multiple_of(2)
        || !value.is_ascii()
        || value.len() > 32768
    {
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
    use crate::manifest::{
        CheckPlan, ContainerServicePlan, GrpcOperationPlan, GrpcServingStatus, ShellOperationPlan,
    };
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
        assert_eq!(bundle.replay_plan.format, EXECUTION_REPLAY_FORMAT);
    }

    fn execution_api_fixture(mode: &str) -> tempfile::TempDir {
        let directory = fixture();
        let script = include_str!("../tests/fixtures/execution_api.py")
            .replace("MODE = \"exit\"", &format!("MODE = {mode:?}"));
        fs::write(directory.path().join("runtime/firecracker"), script).unwrap();
        let manifest = directory.path().join("theseus.toml");
        let text = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            format!(
                "{text}\ntimeout_secs = 1\n\n[[events]]\nwhen = \"ready\"\ndata = \"32312e35430a\"\n"
            ),
        )
        .unwrap();
        directory
    }

    #[test]
    fn api_bundle_replays_its_exact_stream_after_the_source_is_removed() {
        let directory = execution_api_fixture("exit");
        let root = directory.path();
        let bundle = root.join("run");
        test(root.join("theseus.toml"), &bundle).unwrap();
        fs::remove_dir_all(root.join("runtime")).unwrap();
        fs::remove_dir_all(root.join("guest")).unwrap();
        fs::remove_file(root.join("theseus.toml")).unwrap();
        let rerun = root.join("rerun");
        replay_to(&bundle, &rerun).unwrap();
        assert_eq!(
            fs::read(bundle.join("execution.json")).unwrap(),
            fs::read(rerun.join("execution.json")).unwrap()
        );
        let result: Value =
            serde_json::from_slice(&fs::read(rerun.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["execution_evidence"], "execution.json");
        assert!(result["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |check| check["name"] == "replay_machine_execution" && check["status"] == "passed"
            ));
        assert!(matches!(
            replay_to(&bundle, &rerun),
            Err(RunError::BundleExists(_))
        ));
        fs::remove_file(bundle.join("execution.json")).unwrap();
        fs::remove_file(bundle.join("result.json")).unwrap();
        let rejected = root.join("missing-evidence");
        assert!(replay_to(&bundle, &rejected).is_err());
        assert!(!rejected.exists());
    }

    #[test]
    fn changed_uart_input_fails_before_delivery_and_retains_diagnostics() {
        let directory = execution_api_fixture("exit");
        let root = directory.path();
        let bundle = root.join("run");
        test(root.join("theseus.toml"), &bundle).unwrap();
        let mut plan = read_plan(&bundle.join("replay-plan.json")).unwrap();
        plan.events[0].data_hex = "32322e35430a".into();
        write_json(&bundle.join("replay-plan.json"), &plan).unwrap();
        let rerun = root.join("divergence");
        let error = replay_to(&bundle, &rerun).unwrap_err();
        assert!(error.to_string().contains("evidence:"));
        let evidence = crate::execution::Evidence::read(&rerun.join("execution.json"), 1).unwrap();
        assert!(evidence.machine_execution_trace.is_empty());
        assert!(evidence.replay_error.unwrap().contains("decision 0"));
        assert!(!fs::read_to_string(rerun.join("serial.log"))
            .unwrap()
            .contains("sensor reading:"));
        let result: Value =
            serde_json::from_slice(&fs::read(rerun.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["status"], "failed");
    }

    #[test]
    fn timeout_retains_a_paused_cut_and_never_claims_active_replay() {
        let directory = execution_api_fixture("pause");
        let root = directory.path();
        let bundle = root.join("run");
        assert!(test(root.join("theseus.toml"), &bundle).is_err());
        let evidence = crate::execution::Evidence::read(&bundle.join("execution.json"), 1).unwrap();
        assert_eq!(evidence.boundary, "pause");
        let rerun = root.join("rejected");
        assert!(replay_to(&bundle, &rerun)
            .unwrap_err()
            .to_string()
            .contains("host-timed pause"));
        assert!(!rerun.exists());
    }

    #[test]
    fn bundle_locks_a_container_image_and_its_adapter() {
        let directory = fixture();
        let root = directory.path();
        let adapter = root.join("runtime/theseus-image");
        fs::write(&adapter, b"adapter").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("guest/service.tar"), b"container image").unwrap();
        fs::write(
            root.join("theseus.toml"),
            r#"version = 1
[runtime]
firecracker = "runtime/firecracker"
image_adapter = "runtime/theseus-image"
[guest]
kernel = "guest/vmlinux"
image = "guest/service.tar"
[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128
"#,
        )
        .unwrap();
        let plan = load_plan(root.join("theseus.toml")).unwrap();
        let output = root.join("replay");
        let bundle = Bundle::create(&output, root.join("theseus.toml").as_path(), &plan).unwrap();

        assert_eq!(
            fs::read(output.join("artifacts/image.tar")).unwrap(),
            b"container image"
        );
        assert_eq!(
            bundle.replay_plan.guest.image.as_ref().unwrap().path,
            "artifacts/image.tar"
        );
        assert_eq!(
            bundle
                .replay_plan
                .runtime
                .image_adapter
                .as_ref()
                .unwrap()
                .path,
            "artifacts/theseus-image"
        );
        validate_replay_plan(&output.join("replay-plan.json"), &bundle.replay_plan).unwrap();
    }

    #[test]
    fn materializes_a_container_image_with_its_locked_adapter() {
        let directory = fixture();
        let root = directory.path();
        let adapter = root.join("runtime/theseus-image");
        fs::write(
            &adapter,
            "#!/bin/sh\n[ \"$1\" = flatten ] && [ \"$3\" = --output ] && [ \"$5\" = --service ]\ngrep -q '127.0.0.1:8080/health' \"$6\"\ncp \"$2\" \"$4\"\n",
        )
        .unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("guest/service.tar"), b"flattened image").unwrap();
        fs::write(
            root.join("theseus.toml"),
            r#"version = 1
[runtime]
firecracker = "runtime/firecracker"
image_adapter = "runtime/theseus-image"
[guest]
kernel = "guest/vmlinux"
image = "guest/service.tar"
[run]
seed = 42
vcpu_count = 1
mem_size_mib = 128

[container_service.ready]
url = "http://127.0.0.1:8080/health"

[[container_service.assertions]]
name = "health"
url = "http://127.0.0.1:8080/health"
body_contains = "ok"
"#,
        )
        .unwrap();
        let plan = load_plan(root.join("theseus.toml")).unwrap();
        let output = root.join("run");
        fs::create_dir(&output).unwrap();

        let initramfs = materialize_initramfs(&plan, root, &output).unwrap();
        assert_eq!(fs::read(initramfs).unwrap(), b"flattened image");
        assert!(output.join("container-service.json").is_file());
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
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = vec![0; b"PUT /entropy HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"seed\":42}".len()];
            stream.read_exact(&mut request).unwrap();
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("PUT /entropy HTTP/1.1"));
            assert!(request.contains("{\"seed\":42}"));
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            // Keep the server's write side open until the client has consumed
            // the framed response and dropped its connection.
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });

        api_put(&socket, "/entropy", json!({ "seed": 42 })).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn api_response_reads_framed_faults_and_rejects_invalid_lengths() {
        for (response, expected) in [
            (
                "HTTP/1.1 400 Bad Request\r\ncontent-length: 5\r\n\r\nfault",
                Some("fault"),
            ),
            ("HTTP/1.1 400 Bad Request\r\n\r\n", None),
            (
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
                None,
            ),
            (
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 131073\r\n\r\n",
                None,
            ),
            (
                "HTTP/1.1 204 No Content\r\nTransfer-Encoding: chunked\r\n\r\n",
                None,
            ),
            ("HTTP/1.1 2040 Invalid\r\nContent-Length: 0\r\n\r\n", None),
        ] {
            let (client, mut server) = UnixStream::pair().unwrap();
            server.write_all(response.as_bytes()).unwrap();
            let result = read_api_response(client);
            if let Some(body) = expected {
                assert!(result.unwrap().ends_with(body));
            } else {
                assert!(result.is_err(), "accepted {response:?}");
            }
        }
    }

    #[test]
    fn properties_evaluate_serial_text_and_markers_with_builtin_outcomes() {
        let directory = fixture();
        let mut plan = load_plan(directory.path().join("theseus.toml")).unwrap();
        plan.checks = vec![
            CheckPlan {
                name: "finished".to_owned(),
                kind: CheckKind::SerialContains,
                value: "finished work".to_owned(),
            },
            CheckPlan {
                name: "no panic".to_owned(),
                kind: CheckKind::SerialNotContains,
                value: "panic".to_owned(),
            },
            CheckPlan {
                name: "ready".to_owned(),
                kind: CheckKind::MarkerSeen,
                value: "42".to_owned(),
            },
            CheckPlan {
                name: "no error".to_owned(),
                kind: CheckKind::MarkerNotSeen,
                value: "ee".to_owned(),
            },
        ];
        let serial_log = directory.path().join("serial.log");
        fs::write(&serial_log, b"THES:M:42\nfinished work\n").unwrap();

        let execution = evaluate_checks(&plan, &serial_log, Terminal::TimedOut).unwrap();
        assert!(!execution.passed());
        assert_eq!(execution.checks[0].name, "guest_exit");
        assert_eq!(execution.checks[0].status, "failed");
        assert_eq!(execution.checks[1].name, "completion");
        assert!(execution.checks[2..]
            .iter()
            .all(|check| check.status == "passed"));
    }

    #[test]
    fn recognizes_a_grpc_health_operation_in_serial_evidence() {
        let directory = fixture();
        let mut plan = load_plan(directory.path().join("theseus.toml")).unwrap();
        plan.container_service = Some(ContainerServicePlan {
            campaign: false,
            ready: None,
            assertions: Vec::new(),
            operations: Vec::new(),
            grpc_ready: None,
            grpc_assertions: Vec::new(),
            grpc_operations: vec![GrpcOperationPlan {
                name: "api_health".to_owned(),
                url: "http://127.0.0.1:50051".to_owned(),
                service: "example.Api".to_owned(),
                expect_status: GrpcServingStatus::Serving,
            }],
            shell_operations: vec![ShellOperationPlan {
                name: "read_health".to_owned(),
                command: vec!["/bin/cat".to_owned(), "/health".to_owned()],
                expect_exit: 0,
                output_contains: Some("ok".to_owned()),
                output_json: false,
                environment: std::collections::BTreeMap::new(),
            }],
        });
        let serial_log = directory.path().join("serial.log");
        fs::write(
            &serial_log,
            b"THES:GRPC:operation:api_health:PASS\nTHES:SHELL:operation:read_health:PASS\n",
        )
        .unwrap();

        let execution = evaluate_checks(&plan, &serial_log, Terminal::TimedOut).unwrap();
        assert!(execution.checks.iter().any(|check| {
            check.name == "container_service.operation.api_health" && check.status == "passed"
        }));
        assert!(execution.checks.iter().any(|check| {
            check.name == "container_service.operation.read_health" && check.status == "passed"
        }));
    }
}
