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

    let forked = directory.path().join("forked");
    fs::create_dir(&forked).unwrap();
    fs::write(
        forked.join("campaign-result.json"),
        r#"{"counterfactual":{"run":0,"fault":"backplane:partition@read","replace":"backplane:heal@read"},"runs":[{"index":0,"operations":["read"],"faults":["backplane:heal@read"],"state_sha256":"forked-state","timeline":[{"operation":"read","service":"api","state_sha256":"forked-state","moment":"9000@fork-hash","program_counters":{"api":["0x8010"]}}]}],"properties":[{"name":"consistent_read","kind":"always","status":"passed"}]}"#,
    )
    .unwrap();
    let comparison = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["compare", "--forked", "before", "forked"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(comparison.status.success(), "{comparison:?}");
    let comparison: serde_json::Value = serde_json::from_slice(&comparison.stdout).unwrap();
    assert_eq!(comparison["status"], "diverged");
    assert_eq!(comparison["forked_run"], 0);
    assert_eq!(comparison["replaced_fault"], "backplane:partition@read");
    assert_eq!(comparison["replacement_fault"], "backplane:heal@read");
    assert_eq!(
        comparison["divergence"]["reason"],
        "operation-boundary state diverges"
    );
    assert_eq!(comparison["divergence"]["moments"][0], "");
    assert_eq!(comparison["divergence"]["moments"][1], "9000@fork-hash");

    // A comparison between two forks is rejected instead of guessed.
    let ambiguous = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["compare", "--forked", "forked", "forked"])
        .current_dir(directory.path())
        .status()
        .unwrap();
    assert!(!ambiguous.success(), "{ambiguous:?}");
}

#[test]
fn query_resolves_temporal_relations_over_retained_moments() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("campaign-result.json"),
        r#"{"runs":[{"index":0,"timeline":[
            {"id":"op-000-write","operation":"write","service":"api","moment":"7000@input-hash","serial_delta":{"api":{"bytes":16,"sha256":"h0","excerpt":"write\ncomplete\n","omitted_bytes":0}}},
            {"id":"op-001-read","operation":"read","service":"counter","moment":"9000@read-hash","serial_delta":{"counter":{"bytes":11,"sha256":"h1","excerpt":"THES:M:stale\n","omitted_bytes":0}}},
            {"id":"op-002-verify","operation":"verify","service":"api","moment":"12000@verify-hash","serial_delta":{"api":{"bytes":8,"sha256":"h2","excerpt":"done\n","omitted_bytes":0}}}
        ]}]}"#,
    )
    .unwrap();

    let json = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["query", ".", "--followed-by", "stale", "--format", "json"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(json.status.success(), "{json:?}");
    let answer: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(answer["format"], "theseus-query-temporal-v1");
    assert_eq!(answer["relation"], "followed_by");
    assert_eq!(answer["occurrences"][0]["boundary"], "op-001-read");
    assert_eq!(answer["occurrences"][0]["service"], "counter");
    assert_eq!(answer["matches"][0]["boundary"], "op-000-write");
    assert_eq!(answer["matches"][0]["moment"], "7000@input-hash");

    let text = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args([
            "query",
            ".",
            "--preceded-by",
            "stale",
            "--service",
            "counter",
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(text.status.success(), "{text:?}");
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("relation: preceded_by"), "{text}");
    assert!(
        text.contains("occurrence\t9000@read-hash\t0\top-001-read\tcounter"),
        "{text}"
    );
    // The marker's own boundary is a counter boundary, but it never matches
    // its own occurrence, and no later counter boundary exists.
    assert!(!text.contains("\nmatch\t"), "{text}");

    // Empty needles and flag combinations are usage errors.
    for args in [
        vec!["query", ".", "--preceded-by", ""],
        vec![
            "query",
            ".",
            "--preceded-by",
            "x",
            "--moment",
            "7000@input-hash",
        ],
        vec!["query", ".", "--followed-by", "x", "--list"],
    ] {
        let bad = Command::new(env!("CARGO_BIN_EXE_theseus"))
            .args(&args)
            .current_dir(directory.path())
            .status()
            .unwrap();
        assert!(!bad.success(), "{args:?}");
    }
}

#[test]
fn query_collects_a_self_contained_artifact_bundle_for_one_moment() {
    use sha2::{Digest, Sha256};
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("campaign");
    let serial = b"READYlog output\n";
    fs::create_dir_all(bundle.join("runs/000/services/api")).unwrap();
    let result = serde_json::json!({
        "runs": [{"index": 0, "decision_trace": [
            "test_template:main",
            "boundary:0:operation:write",
            "boundary:1:operation:verify"
        ], "timeline": [
            {"id": "op-000-write", "operation": "write", "service": "api",
             "moment": "7000@input-hash",
             "serial_sha256": {"api": format!("{:x}", Sha256::digest(&serial[..5]))},
             "serial_delta": {"api": {"bytes": 5, "sha256": "d0", "excerpt": "READY", "omitted_bytes": 0}}},
            {"id": "op-002-verify", "operation": "verify", "service": "api",
             "moment": "12000@verify-hash",
             "serial_sha256": {"api": format!("{:x}", Sha256::digest(serial))},
             "serial_delta": {"api": {"bytes": serial.len() - 5, "sha256": "d1", "excerpt": "log output", "omitted_bytes": 0}},
             "actions": [{"kind": "custom"}]}
        ]}]
    });
    fs::write(
        bundle.join("campaign-result.json"),
        serde_json::to_string(&result).unwrap(),
    )
    .unwrap();
    fs::write(bundle.join("runs/000/services/api/serial.log"), serial).unwrap();

    let collected = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args([
            "query",
            "campaign",
            "--moment",
            "12000@verify-hash",
            "--collect",
        ])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(collected.status.success(), "{collected:?}");
    let text = String::from_utf8(collected.stdout).unwrap();
    assert!(text.contains("collected: campaign-collected"), "{text}");
    assert!(text.contains("boundary: op-002-verify"), "{text}");
    assert!(text.contains("serial_slices: collected"), "{text}");

    // The default output sits beside the untouched source bundle, and the
    // manifest digest-matches every file it lists.
    let output = directory.path().join("campaign-collected");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], "theseus-collected-artifacts-v1");
    assert!(manifest["source"].as_str().unwrap().ends_with("/campaign"));
    for entry in manifest["files"].as_array().unwrap() {
        let bytes = fs::read(output.join(entry["path"].as_str().unwrap())).unwrap();
        assert_eq!(
            entry["sha256"],
            format!("{:x}", Sha256::digest(&bytes)),
            "{}",
            entry["path"]
        );
    }
    assert_eq!(fs::read(output.join("serial/api.log")).unwrap(), serial);
    let boundary: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("boundary.json")).unwrap()).unwrap();
    assert_eq!(boundary["actions"][0]["kind"], "custom");
    assert_eq!(
        fs::read_dir(&bundle).unwrap().count(),
        2,
        "runs/ and campaign-result.json only: the source stays read-only"
    );

    // An existing output refuses collection, and flag combinations fail.
    let again = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args([
            "query",
            "campaign",
            "--moment",
            "12000@verify-hash",
            "--collect",
        ])
        .current_dir(directory.path())
        .status()
        .unwrap();
    assert!(!again.success(), "{again:?}");
    let bad = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args([
            "query",
            "campaign",
            "--moment",
            "12000@verify-hash",
            "--collect",
            "--list",
        ])
        .current_dir(directory.path())
        .status()
        .unwrap();
    assert!(!bad.success(), "{bad:?}");
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
        r#"{"format":"theseus-compose-plan-v1","services":{"api":{}}}"#,
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
        r#"{"format":"theseus-compose-plan-v1","services":{"api":{}}}"#,
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
    assert!(help.contains("compare --forked base-campaign-dir forked-campaign-dir"));
    assert!(help.contains("--fork-run N --replace-fault OLD=NEW campaign-dir"));
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

#[test]
fn status_summarizes_a_retained_campaign_without_kvm() {
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("campaign");
    fs::create_dir_all(bundle.join("runs/000")).unwrap();
    fs::create_dir_all(bundle.join("runs/001")).unwrap();
    fs::write(
        bundle.join("campaign-result.json"),
        r#"{"format":"theseus-compose-campaign-result-v1","status":"failed",
            "driver":"api","guidance":"unified","coverage":"execution_locations",
            "runs":[{"index":0,"status":"passed"},{"index":1,"status":"failed"}],
            "properties":[{"name":"lost_update","kind":"always","status":"failed","detail":"0 of 2 retained timelines satisfied the serial needle"}]}"#,
    )
    .unwrap();
    fs::write(
        bundle.join("replay-plan.json"),
        r#"{"format":"theseus-compose-plan-v1","campaign":{"driver":"api","max_runs":64}}"#,
    )
    .unwrap();

    let json = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["status", "campaign", "--format", "json"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(json.status.success(), "{json:?}");
    let status: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(status["format"], "theseus-campaign-status-v1");
    assert_eq!(status["status"], "failed");
    assert_eq!(status["driver"], "api");
    assert_eq!(status["budget"], 64);
    assert_eq!(status["run_count"], 2);
    assert_eq!(status["failed_runs"][0], 1);
    assert_eq!(status["failed_properties"][0], "lost_update");
    assert_eq!(status["properties"][0]["name"], "lost_update");
    assert_eq!(status["artifacts"]["runs"], 2);
    assert_eq!(status["artifacts"]["checkpoint"], false);

    let text = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["status", "campaign"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(text.status.success(), "{text:?}");
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("status: failed"), "{text}");
    assert!(text.contains("failed properties: lost_update"), "{text}");
    assert!(
        text.contains("property lost_update (always): failed"),
        "{text}"
    );
    assert!(
        text.contains("artifacts: result true plan true runs 2 checkpoint false"),
        "{text}"
    );

    // Directories without campaign evidence name their emptiness.
    let empty = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["status", "."])
        .current_dir(directory.path())
        .status()
        .unwrap();
    assert!(!empty.success(), "{empty:?}");

    // The help surface lists the command beside the comparison surface.
    let help = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("theseus status campaign-dir"), "{help}");
}

#[test]
fn coverage_java_builds_an_agent_that_reports_the_locked_points() {
    // The Java path needs a JDK with javac, jar, and java on PATH, exactly
    // like the Go frontend needs a Go toolchain.
    if Command::new("javac").arg("--version").output().is_err() {
        panic!("the Java coverage integration test needs a JDK on PATH");
    }
    let directory = tempfile::tempdir().unwrap();
    let work = directory.path().join("work");
    fs::create_dir_all(work.join("src/com/example")).unwrap();
    fs::write(
        work.join("src/com/example/Hello.java"),
        "package com.example;\npublic class Hello {\n    public static void main(String[] args) {\n        System.out.println(\"hello from theseus\");\n    }\n}\n",
    )
    .unwrap();
    let compiled = Command::new("javac")
        .args(["-d", "classes", "src/com/example/Hello.java"])
        .current_dir(&work)
        .status()
        .unwrap();
    assert!(compiled.success(), "{compiled:?}");
    let archived = Command::new("jar")
        .args(["--create", "--file", "app.jar", "-C", "classes", "com"])
        .current_dir(&work)
        .status()
        .unwrap();
    assert!(archived.success(), "{archived:?}");

    let built = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args([
            "coverage",
            "java",
            "--process",
            "api",
            "--module",
            "app",
            "--jar",
            "app.jar",
            "--symbols",
            "symbols",
            "--output",
            "app.theseus-coverage.json",
        ])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(built.status.success(), "{built:?}");

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(work.join("app.theseus-coverage.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], "theseus-java-coverage-build-v1");
    assert_eq!(manifest["coverage"], "classes");
    assert_eq!(manifest["language"], "java");
    assert_eq!(manifest["module"], "app");
    let build_sha256 = manifest["build_sha256"].as_str().unwrap().to_owned();
    assert_eq!(build_sha256.len(), 64);
    let symbols: serde_json::Value = serde_json::from_slice(
        &fs::read(
            work.join("symbols")
                .join(manifest["symbols"].as_str().unwrap()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(symbols["format"], "theseus-java-coverage-symbols-v1");
    assert_eq!(symbols["build_sha256"], build_sha256.as_str());
    let classes = symbols["classes"].as_array().unwrap();
    let app = classes
        .iter()
        .find(|class| class["class"] == "com/example/Hello")
        .unwrap_or_else(|| panic!("symbol map misses the class: {symbols}"));
    let offset = app["offset"].as_str().unwrap();

    // The agent re-derives the same coverage point at runtime and reports
    // it through the shared first-hit serial-line protocol.
    let agent_jar = work.join("theseus-coverage-agent.jar");
    assert!(agent_jar.is_file());
    let run = Command::new("java")
        .arg(format!(
            "-javaagent:{}=api,app,{build_sha256}",
            agent_jar.display()
        ))
        .args(["-cp", "app.jar", "com.example.Hello"])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(run.status.success(), "{run:?}");
    let stderr = String::from_utf8_lossy(&run.stderr);
    let expected = format!("THES:COV:v1:api:app:{build_sha256}:{offset}");
    assert!(
        stderr.lines().any(|line| line.trim() == expected),
        "expected {expected} in:\n{stderr}"
    );
    // First-hit: the same class is never reported twice.
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("THES:COV:"))
            .count(),
        1
    );

    // Unknown flags are usage errors.
    let bad = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["coverage", "java", "--nonsense"])
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(!bad.status.success(), "{bad:?}");
    assert!(String::from_utf8_lossy(&bad.stderr).contains("Usage:"));
}

#[test]
fn history_traces_property_verdicts_across_campaigns() {
    let directory = tempfile::tempdir().unwrap();
    let plan = r#"{"format":"theseus-compose-plan-v1","campaign":{"driver":"api","properties":[
        {"name":"lost_update","kind":"always","contains":"THES:ASSERT:no_data_loss:pass"}]}}"#;
    for (name, status, run_status) in [
        ("before", "failed", "failed"),
        ("after", "passed", "passed"),
    ] {
        let bundle = directory.path().join(name);
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("campaign-result.json"), format!(r#"{{"status":"{status}","runs":[{{"index":0,"status":"{run_status}"}}],"properties":[{{"name":"lost_update","kind":"always","status":"{status}","detail":"retained verdict"}}]}}"#)).unwrap();
        fs::write(bundle.join("replay-plan.json"), plan).unwrap();
    }

    let json = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["history", "before", "after", "--format", "json"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(json.status.success(), "{json:?}");
    let history: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(history["format"], "theseus-campaign-property-history-v1");
    assert_eq!(history["properties"].as_array().unwrap().len(), 1);
    let entry = &history["properties"][0];
    assert_eq!(entry["name"], "lost_update");
    assert!(entry["declaration_sha256"].as_str().unwrap().len() == 64);
    assert_eq!(entry["verdicts"].as_array().unwrap().len(), 2);
    assert_eq!(entry["verdicts"][0]["status"], "failed");
    assert_eq!(entry["verdicts"][1]["status"], "passed");
    assert!(entry["first_failed_source"]
        .as_str()
        .unwrap()
        .ends_with("/before"));

    let text = Command::new(env!("CARGO_BIN_EXE_theseus"))
        .args(["history", "before", "after", "--property", "lost_update"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(text.status.success(), "{text:?}");
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains("property lost_update (always) declaration"),
        "{text}"
    );
    assert!(text.contains("verdict\tfailed\t"), "{text}");
    assert!(text.contains("verdict\tpassed\t"), "{text}");
    assert!(text.contains("first failed: "), "{text}");

    // No sources and evidence-free sources are rejected.
    for args in [vec!["history"], vec!["history", "missing-bundle"]] {
        let bad = Command::new(env!("CARGO_BIN_EXE_theseus"))
            .args(&args)
            .current_dir(directory.path())
            .status()
            .unwrap();
        assert!(!bad.success(), "{args:?}");
    }
}
