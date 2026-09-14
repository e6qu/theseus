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

#[cfg(target_os = "linux")]
#[test]
fn cargo_coverage_instruments_a_workspace_dependency_graph() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("app/src")).unwrap();
    fs::create_dir_all(directory.path().join("logic/src")).unwrap();
    fs::create_dir_all(directory.path().join("devtool/src")).unwrap();
    fs::write(
        directory.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"logic\", \"devtool\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("app/Cargo.toml"),
        "[package]\nname = \"classifier\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nlogic = { path = \"../logic\" }\n\n[dev-dependencies]\ndevtool = { path = \"../devtool\" }\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("app/src/main.rs"),
        "fn main() { let value = std::env::args().nth(1).unwrap().parse().unwrap(); println!(\"{}\", logic::classify(value)); }\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("logic/Cargo.toml"),
        "[package]\nname = \"logic\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("logic/src/lib.rs"),
        "#[inline(never)]\npub fn classify(value: u32) -> &'static str { if value == 7 { \"seven\" } else if value % 2 == 0 { \"even\" } else { \"odd\" } }\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("logic/build.rs"),
        "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("devtool/Cargo.toml"),
        "[package]\nname = \"devtool\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("devtool/src/lib.rs"),
        "pub fn test_only() {}\n",
    )
    .unwrap();

    let build = || {
        Command::new(env!("CARGO_BIN_EXE_theseus"))
            .args([
                "coverage",
                "cargo",
                "--process",
                "classifier",
                "--module",
                "command",
                "--package",
                "classifier",
                "--bin",
                "classifier",
                "--manifest-path",
                "Cargo.toml",
                "--symbols",
                "out/symbols",
                "--output",
                "out/classifier",
                "--offline",
            ])
            .current_dir(directory.path())
            .output()
            .unwrap()
    };
    let first = build();
    assert!(first.status.success(), "{first:?}");
    let manifest_path = directory
        .path()
        .join("out/classifier.theseus-coverage.json");
    let first_manifest = fs::read(&manifest_path).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&first_manifest).unwrap();
    assert_eq!(manifest["maximum_edges"], 65_535);
    assert_eq!(manifest["cargo"]["packages"], 2);
    assert_eq!(
        manifest["cargo"]["rust_target_dependencies_instrumented"],
        true
    );
    assert_eq!(manifest["cargo"]["host_build_targets_instrumented"], false);
    assert_eq!(
        manifest["cargo"]["dynamic_rust_targets_instrumented"],
        false
    );
    assert_eq!(
        manifest["cargo"]["workspace_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(manifest["sources"][0]["name"], "classifier");
    assert_eq!(manifest["sources"][1]["name"], "logic");

    let run = Command::new(directory.path().join("out/classifier"))
        .arg("7")
        .output()
        .unwrap();
    assert!(run.status.success(), "{run:?}");
    assert_eq!(run.stdout, b"seven\n");
    let records = String::from_utf8(run.stderr).unwrap();
    assert!(records.contains("THES:COV:v2:classifier:command:"));

    let symbols = directory
        .path()
        .join("out/symbols")
        .join(manifest["symbols"].as_str().unwrap());
    let inspect = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("instrumentation/llvm/theseus-coverage-inspect");
    let mut locations = String::new();
    for offset in records
        .lines()
        .filter_map(|line| line.rsplit(':').next())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let result = Command::new(&inspect)
            .args([symbols.as_os_str(), manifest_path.as_os_str()])
            .arg(offset)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        locations.push_str(&String::from_utf8(result.stdout).unwrap());
    }
    assert!(locations.contains("logic/src/lib.rs"), "{locations}");

    let second = build();
    assert!(second.status.success(), "{second:?}");
    assert_eq!(fs::read(&manifest_path).unwrap(), first_manifest);

    fs::write(
        directory.path().join("logic/src/lib.rs"),
        "#[inline(never)]\npub fn classify(value: u32) -> &'static str { if value == 7 { \"lucky\" } else if value % 2 == 0 { \"even\" } else { \"odd\" } }\n",
    )
    .unwrap();
    let changed = build();
    assert!(changed.status.success(), "{changed:?}");
    let changed_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert_ne!(changed_manifest["build_sha256"], manifest["build_sha256"]);
}

#[cfg(target_os = "linux")]
#[test]
fn go_coverage_instruments_a_module_dependency_graph() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("cmd/classifier")).unwrap();
    fs::create_dir_all(directory.path().join("logic")).unwrap();
    fs::create_dir_all(directory.path().join("testonly")).unwrap();
    fs::create_dir_all(directory.path().join("third_party/labels")).unwrap();
    fs::write(
        directory.path().join("go.mod"),
        "module example.com/classifier\n\ngo 1.19\n\nrequire example.com/labels v0.0.0\n\nreplace example.com/labels => ./third_party/labels\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("cmd/classifier/main.go"),
        r#"package main

import (
	"fmt"
	"os"
	"strconv"

	"example.com/classifier/logic"
)

func main() {
	value, _ := strconv.Atoi(os.Args[1])
	fmt.Println(logic.Classify(value))
}
"#,
    )
    .unwrap();
    let logic = r#"package logic

import "example.com/labels"

func Classify(value int) string {
	if value == 7 {
		return labels.Seven()
	}
	if value%2 == 0 {
		return "even"
	}
	return "odd"
}
"#;
    fs::write(directory.path().join("logic/logic.go"), logic).unwrap();
    fs::write(
        directory.path().join("third_party/labels/go.mod"),
        "module example.com/labels\n\ngo 1.19\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("third_party/labels/labels.go"),
        "package labels\n\nfunc Seven() string { return \"seven\" }\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("testonly/helper.go"),
        "package testonly\n\nfunc Helper() {}\n",
    )
    .unwrap();

    let build = || {
        Command::new(env!("CARGO_BIN_EXE_theseus"))
            .args([
                "coverage",
                "go",
                "--process",
                "classifier",
                "--module",
                "command",
                "--package",
                "./cmd/classifier",
                "--symbols",
                "out/symbols",
                "--output",
                "out/classifier",
                "--offline",
            ])
            .env("GOCACHE", "/proc/theseus-go-cache-must-not-be-used")
            .current_dir(directory.path())
            .output()
            .unwrap()
    };
    let first = build();
    assert!(first.status.success(), "{first:?}");
    let manifest_path = directory
        .path()
        .join("out/classifier.theseus-coverage.json");
    let first_manifest = fs::read(&manifest_path).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&first_manifest).unwrap();
    assert_eq!(manifest["format"], "theseus-go-coverage-build-v1");
    assert_eq!(manifest["coverage"], "blocks");
    assert_eq!(manifest["language"], "go");
    assert_eq!(
        manifest["go"]["package"],
        "example.com/classifier/cmd/classifier"
    );
    assert_eq!(manifest["go"]["packages"], 3);
    assert_eq!(manifest["go"]["instrumented_packages"], 2);
    assert_eq!(manifest["go"]["cgo_enabled"], false);
    assert!(manifest["maximum_blocks"].as_u64().unwrap() >= 5);
    let external = manifest["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["import_path"] == "example.com/labels")
        .unwrap();
    assert_eq!(external["instrumented"], false);
    assert_eq!(
        fs::read_to_string(directory.path().join("logic/logic.go")).unwrap(),
        logic
    );
    assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".theseus-go-")
    }));

    let binary = directory.path().join("out/classifier");
    let symbols = directory
        .path()
        .join("out/symbols")
        .join(manifest["symbols"].as_str().unwrap());
    let inspector = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("instrumentation/go/theseus-coverage-inspect");
    let mut locations = String::new();
    for value in ["7", "2", "3"] {
        let run = Command::new(&binary).arg(value).output().unwrap();
        assert!(run.status.success(), "{run:?}");
        let records = String::from_utf8(run.stderr).unwrap();
        assert!(records.contains("THES:COV:v1:classifier:command:"));
        for counter in records
            .lines()
            .filter_map(|line| line.rsplit(':').next())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let inspected = Command::new(&inspector)
                .args([symbols.as_os_str(), manifest_path.as_os_str()])
                .arg(counter)
                .output()
                .unwrap();
            assert!(inspected.status.success(), "{inspected:?}");
            locations.push_str(&String::from_utf8(inspected.stdout).unwrap());
        }
    }
    assert!(locations.contains("logic/logic.go"), "{locations}");
    assert!(locations.contains("cmd/classifier/main.go"), "{locations}");

    let second = build();
    assert!(second.status.success(), "{second:?}");
    assert_eq!(fs::read(&manifest_path).unwrap(), first_manifest);

    fs::write(
        directory.path().join("logic/logic.go"),
        logic.replace("return \"odd\"", "return \"other\""),
    )
    .unwrap();
    let changed = build();
    assert!(changed.status.success(), "{changed:?}");
    let changed_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert_ne!(changed_manifest["build_sha256"], manifest["build_sha256"]);
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
        .contains("First recorded divergence"));

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
        r#"version = 2
name = "public corpus"
lockfile = "theseus-evaluation.lock"
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
    fs::write(
        bundle.join("replay-plan.json"),
        r#"{"format":"theseus-compose-plan-v1","services":[{}]}"#,
    )
    .unwrap();
    let lock = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["evaluate", "lock", "theseus-evaluation.toml"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(lock.status.success(), "{lock:?}");

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
    assert!(output.contains("Locked artifacts: verified (2 files"));
}

#[test]
fn evaluate_capture_publishes_a_complete_campaign_without_kvm() {
    let directory = tempfile::tempdir().unwrap();
    let campaign = directory.path().join("campaign");
    fs::create_dir_all(campaign.join("services/api")).unwrap();
    fs::write(
        campaign.join("replay-plan.json"),
        r#"{"format":"theseus-compose-plan-v1","services":[{}]}"#,
    )
    .unwrap();
    fs::write(campaign.join("services/api/serial.log"), "evidence\n").unwrap();
    fs::write(
        campaign.join("campaign-result.json"),
        r#"{"format":"theseus-compose-campaign-result-v1","status":"failed","replay_verification":{"status":"passed"},"runs":[],"properties":[{"name":"consistent_read","status":"failed"}]}"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args([
            "evaluate",
            "capture",
            "campaign",
            "--output",
            "public",
            "--name",
            "counter failure",
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(directory
        .path()
        .join("public/campaign/services/api/serial.log")
        .is_file());

    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["evaluate", "public/theseus-evaluation.toml"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("\"status\": \"passed\""));
}

#[test]
fn versioned_replicated_counter_fixture_stays_explicitly_unverified() {
    let evaluation = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../evaluations/replicated-counter/theseus-evaluation.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["evaluate", evaluation.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["replay"]["verified"], 0);
    assert!(report.get("conventional_baseline").is_none());
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
    assert!(help.contains("evaluate lock [theseus-evaluation.toml]"));
    assert!(help.contains("evaluate capture campaign-dir --output evaluation-dir --name name"));
    assert!(help.contains("evidence verify native-evidence.json"));
    assert!(help.contains("coverage cargo --process NAME"));
    assert!(help.contains("coverage go --process NAME"));

    let coverage = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["coverage", "cargo", "--help"])
        .output()
        .unwrap();
    assert!(coverage.status.success(), "{coverage:?}");
    assert!(String::from_utf8(coverage.stdout)
        .unwrap()
        .contains("--target-dir DIR"));

    let coverage = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["coverage", "go", "--help"])
        .output()
        .unwrap();
    assert!(coverage.status.success(), "{coverage:?}");
    assert!(String::from_utf8(coverage.stdout)
        .unwrap()
        .contains("--goarch amd64|arm64"));
}
