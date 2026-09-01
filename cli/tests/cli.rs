// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fs;
use std::process::Command;

fn test_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("runtime")).unwrap();
    fs::create_dir_all(directory.path().join("guest")).unwrap();
    fs::write(directory.path().join("runtime/firecracker"), b"firecracker").unwrap();
    #[cfg(unix)]
    fs::set_permissions(
        directory.path().join("runtime/firecracker"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    fs::write(directory.path().join("guest/vmlinux"), b"kernel").unwrap();
    fs::write(directory.path().join("guest/initramfs.cpio"), b"initramfs").unwrap();
    fs::write(
        directory.path().join("theseus.toml"),
        r#"version = 1
[runtime]
firecracker = "runtime/firecracker"
[guest]
kernel = "guest/vmlinux"
initramfs = "guest/initramfs.cpio"
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
fn test_dry_run_prints_a_replayable_plan_without_kvm() {
    let directory = test_directory();
    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["test", "--dry-run", "theseus.toml"])
        .current_dir(directory.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["format"], "theseus-run-plan-v1");
    assert_eq!(plan["run"]["seed"], 42);
}

#[test]
fn report_writes_a_static_page_without_kvm() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("recording");
    fs::create_dir(&input).unwrap();
    fs::write(
        input.join("result.json"),
        r#"{"format":"theseus-result-v1","status":"passed","error":null,"checks":[]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["report", "recording"])
        .current_dir(directory.path())
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(input.join("theseus-report/index.html").is_file());
}
