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

#[test]
fn report_writes_markdown_json_and_junit_without_kvm() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("recording");
    fs::create_dir(&input).unwrap();
    fs::write(
        input.join("result.json"),
        r#"{"format":"theseus-result-v1","status":"failed","error":null,"checks":[{"name":"property","status":"failed","detail":"failed"}]}"#,
    )
    .unwrap();

    for (format, file, expected) in [
        ("markdown", "failure.md", "# Theseus failure report"),
        ("json", "failure.json", "theseus-report-v1"),
        ("junit", "failure.xml", "<testsuite"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
            .args(["report", "--format", format, "--output", file, "recording"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(fs::read_to_string(directory.path().join(file))
            .unwrap()
            .contains(expected));
    }
}

#[test]
fn compare_queries_and_reports_campaign_evidence_without_kvm() {
    let directory = tempfile::tempdir().unwrap();
    for (name, state, pc, property) in [
        ("before", "before-state", "0x8010", "passed"),
        ("after", "after-state", "0x8020", "failed"),
    ] {
        let campaign = directory.path().join(name);
        fs::create_dir(&campaign).unwrap();
        fs::write(
            campaign.join("campaign-result.json"),
            format!(
                r#"{{"runs":[{{"index":0,"operations":["read"],"state_sha256":"{state}","timeline":[{{"operation":"read","service":"api","state_sha256":"{state}","program_counters":{{"api":["{pc}"]}}}}]}}],"properties":[{{"name":"consistent_read","kind":"always","status":"{property}"}}]}}"#
            ),
        )
        .unwrap();
    }

    let compare = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["compare", "before", "after"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(compare.status.success(), "{compare:?}");
    assert!(String::from_utf8(compare.stdout)
        .unwrap()
        .contains("first operation-boundary state differs"));

    let markdown = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["compare", "--format", "markdown", "before", "after"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(markdown.status.success(), "{markdown:?}");
    assert!(String::from_utf8(markdown.stdout)
        .unwrap()
        .contains("First causal divergence"));

    let query = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args([
            "compare",
            "--query",
            "/runs/0/timeline/0/program_counters",
            "before",
            "after",
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(query.status.success(), "{query:?}");
    let query: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(query["left"]["api"][0], "0x8010");
    assert_eq!(query["right"]["api"][0], "0x8020");
}

#[test]
fn evaluate_summarizes_a_locked_public_corpus_without_kvm() {
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("bundle");
    fs::create_dir(&bundle).unwrap();
    fs::write(
        directory.path().join("theseus-evaluation.toml"),
        r#"version = 1
name = "public corpus"
[[workloads]]
name = "counter"
bundle = "bundle"
expected_status = "failed"
[[workloads.properties]]
name = "consistent_read"
status = "failed"
"#,
    )
    .unwrap();
    fs::write(
        bundle.join("campaign-result.json"),
        r#"{"format":"theseus-compose-campaign-result-v1","status":"failed","generated_candidates":4,"unique_topology_states":2,"unique_instruction_locations":3,"replay_verification":{"status":"passed"},"runs":[{"timeline":[{}]}],"properties":[{"name":"consistent_read","status":"failed"}]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args([
            "evaluate",
            "--format",
            "markdown",
            "theseus-evaluation.toml",
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(output.contains("Theseus public evaluation: public corpus"));
    assert!(output.contains("Replay verification: 1/1 bundles"));
}

#[test]
fn versioned_replicated_counter_evaluation_stays_replay_verified() {
    let evaluation = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../evaluations/replicated-counter/theseus-evaluation.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["evaluate", evaluation.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "passed");
    assert_eq!(report["replay"]["verified"], 1);
    assert_eq!(report["conventional_baseline"]["counterexamples"], 0);
}

#[test]
fn evaluate_fails_its_contract_when_replay_evidence_is_missing() {
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("bundle");
    fs::create_dir(&bundle).unwrap();
    fs::write(
        directory.path().join("theseus-evaluation.toml"),
        r#"version = 1
name = "missing replay"
[[workloads]]
name = "counter"
bundle = "bundle"
expected_status = "failed"
"#,
    )
    .unwrap();
    fs::write(
        bundle.join("campaign-result.json"),
        r#"{"format":"theseus-compose-campaign-result-v1","status":"failed"}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["evaluate", "theseus-evaluation.toml"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("\"status\": \"failed\""));
}

#[test]
fn help_lists_bundle_local_replay_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("explore --replay exploration-dir"));
    assert!(help.contains("--seed-path seed,..."));
    assert!(help.contains("explore --minimize exploration-dir"));
    assert!(help.contains("explore --snapshot exploration-dir"));
    assert!(help.contains("compose replay replay-dir"));
    assert!(help.contains("report --format markdown|json|junit"));
    assert!(help.contains("compare --query /json/pointer"));
    assert!(help.contains("evaluate [--format json|markdown]"));
}
