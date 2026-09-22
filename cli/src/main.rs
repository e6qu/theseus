// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use theseus_cli::{
    capture_evaluation, cargo_coverage, cargo_coverage_rustc_wrapper, compare_campaigns, evaluate,
    explore, explore_compose_with, explore_compose_expect_counterexample_with,
    boundary_at_moment, find_moment, go_coverage, list_moments, next_moment_in,
    previous_moment_in,
    load_compose_plan, load_plan, minimize_compose_campaign,
    minimize_compose_campaign_expect_counterexample, minimize_exploration_path, query_campaigns,
    replay, replay_compose, replay_exploration, replay_exploration_path, replay_to, report,
    report_file, report_text, snapshot_exploration_path, test, test_compose,
    verify_native_evidence, verify_topology_bundle, write_evaluation_lock, ReportFormat,
    CARGO_COVERAGE_USAGE, GO_COVERAGE_USAGE,
    CampaignGuidance,
};

const USAGE: &str = "Usage:
  theseus validate [theseus.toml]
  theseus test --dry-run [theseus.toml]
  theseus test [--output replay-dir] [theseus.toml]
  theseus replay replay-dir
  theseus replay --output diagnostics-dir replay-dir
  theseus explore [--output exploration-dir] [theseus.toml]
  theseus explore --replay exploration-dir [--seed-path seed,...] [--output exploration-dir]
  theseus explore --minimize exploration-dir --seed-path seed,... [--output exploration-dir]
  theseus explore --snapshot exploration-dir --seed-path seed,... [--output snapshot-dir]
  theseus report [--output report-dir] result-dir
  theseus report --format markdown|json|junit|github [--output file] result-dir
  theseus compare left-campaign-dir right-campaign-dir
  theseus compare --format json|markdown left-campaign-dir right-campaign-dir
  theseus compare --query /json/pointer left-campaign-dir right-campaign-dir
  theseus compare --at-moment <vtime_ns>@<input_sha256> left-campaign-dir right-campaign-dir
  theseus query campaign-dir --moment <vtime_ns>@<input_sha256> [--next | --previous] [--format json]
  theseus query campaign-dir --list [--service NAME] [--format json]
  theseus evaluate [--format json|markdown] [theseus-evaluation.toml]
  theseus evaluate lock [theseus-evaluation.toml]
  theseus evaluate capture campaign-dir --output evaluation-dir --name name
  theseus evidence verify native-evidence.json
  theseus coverage cargo --process NAME --module NAME --bin NAME --symbols DIR --output FILE
      [--manifest-path Cargo.toml] [--package NAME] [--release] [--locked] [--offline]
      [--no-default-features] [--features FEATURES] [--target-dir DIR]
  theseus coverage go --process NAME --module NAME --package PACKAGE --symbols DIR --output FILE
      [--goarch amd64|arm64] [--tags TAGS] [--mod readonly|vendor] [--offline] [--target-dir DIR]
  theseus compose validate [compose.yaml]
  theseus compose plan [compose.yaml]
  theseus compose test [--output replay-dir] [compose.yaml]
  theseus compose explore [--max-runs N] [--guidance MODE] [--output campaign-dir] [compose.yaml]
  theseus compose explore --expect-counterexample property [--max-runs N] [--guidance MODE] [--output campaign-dir] [compose.yaml]
  theseus compose explore --minimize campaign-dir [--output minimized-dir]
  theseus compose explore --minimize campaign-dir --expect-counterexample property [--output minimized-dir]
  theseus compose replay replay-dir [--output replay-dir]
  theseus compose verify checkpoint-bundle-dir

The manifest path defaults to ./theseus.toml. Relative artifact paths are
resolved from the directory containing that manifest.";

const COMPOSE_USAGE: &str = "Compose accepts a small Theseus-only subset. Each service must set
x-theseus.manifest to a relative theseus.toml path; services join named networks.
Instrumented services may list relative coverage manifests and symbol directories.
Run `theseus compose plan` to inspect the locked service artifacts and links.";

fn manifest_path(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(PathBuf::from("theseus.toml")),
        [path] => Ok(PathBuf::from(path)),
        _ => Err(USAGE.to_owned()),
    }
}

fn seed_path(value: &str) -> Result<Vec<u64>, String> {
    let path = value
        .split(',')
        .map(|seed| seed.parse::<u64>().map_err(|_| USAGE.to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    if path.is_empty() || value.is_empty() {
        return Err(USAGE.to_owned());
    }
    Ok(path)
}

fn print_evaluation(summary: theseus_cli::EvaluationSummary, format: &str) -> Result<(), String> {
    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&summary)
                .map_err(|error| format!("cannot encode evaluation: {error}"))?
        ),
        "markdown" => print!("{}", summary.markdown()),
        _ => return Err("evaluation format must be json or markdown".to_owned()),
    }
    if summary.status == "passed" {
        Ok(())
    } else {
        Err("evaluation did not satisfy its replay or expected-outcome contract".to_owned())
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    match args.as_slice() {
        [command] if command == "--help" || command == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        [command, rest @ ..] if command == "validate" => {
            let path = manifest_path(rest)?;
            let plan = load_plan(&path).map_err(|error| error.to_string())?;
            println!("valid: {}", plan.manifest);
            Ok(())
        }
        [command, flag, rest @ ..] if command == "test" && flag == "--dry-run" => {
            let path = manifest_path(rest)?;
            let plan = load_plan(&path).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&plan)
                    .map_err(|error| format!("could not serialize run plan: {error}"))?
            );
            Ok(())
        }
        [command, flag, output, rest @ ..] if command == "test" && flag == "--output" => {
            let manifest = manifest_path(rest)?;
            let result = test(&manifest, output).map_err(|error| error.to_string())?;
            println!("passed: {}", result.bundle.display());
            Ok(())
        }
        [command, rest @ ..] if command == "test" => {
            let manifest = manifest_path(rest)?;
            let output = manifest
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("theseus-replay");
            let result = test(&manifest, output).map_err(|error| error.to_string())?;
            println!("passed: {}", result.bundle.display());
            Ok(())
        }
        [command, bundle] if command == "replay" => {
            let result = replay(bundle).map_err(|error| error.to_string())?;
            println!("replay passed; logs: {}", result.logs.display());
            Ok(())
        }
        [command, flag, output, bundle] if command == "replay" && flag == "--output" => {
            let result = replay_to(bundle, output).map_err(|error| error.to_string())?;
            println!("replay passed; logs: {}", result.logs.display());
            Ok(())
        }
        [command, format_flag, format, output_flag, output, input]
            if command == "report" && format_flag == "--format" && output_flag == "--output" =>
        {
            let format = ReportFormat::parse(format).ok_or_else(|| {
                "report format must be one of html, markdown, json, or junit".to_owned()
            })?;
            let path = report_file(input, format, output).map_err(|error| error.to_string())?;
            println!("{} report: {}", format.name(), path.display());
            Ok(())
        }
        [command, format_flag, format, input]
            if command == "report" && format_flag == "--format" =>
        {
            let format = ReportFormat::parse(format).ok_or_else(|| {
                "report format must be one of html, markdown, json, or junit".to_owned()
            })?;
            if format == ReportFormat::Html {
                let input = PathBuf::from(input);
                let index = report(&input, input.join("theseus-report"))
                    .map_err(|error| error.to_string())?;
                println!("report: {}", index.display());
            } else {
                print!(
                    "{}",
                    report_text(input, format).map_err(|error| error.to_string())?
                );
            }
            Ok(())
        }
        [command, flag, output, input] if command == "report" && flag == "--output" => {
            let index = report(input, output).map_err(|error| error.to_string())?;
            println!("report: {}", index.display());
            Ok(())
        }
        [command, input] if command == "report" => {
            let input = PathBuf::from(input);
            let index =
                report(&input, input.join("theseus-report")).map_err(|error| error.to_string())?;
            println!("report: {}", index.display());
            Ok(())
        }
        [command, lock] if command == "evaluate" && lock == "lock" => {
            let path = write_evaluation_lock("theseus-evaluation.toml")
                .map_err(|error| error.to_string())?;
            println!("evaluation lock: {}", path.display());
            Ok(())
        }
        [command, lock, input] if command == "evaluate" && lock == "lock" => {
            let path = write_evaluation_lock(input).map_err(|error| error.to_string())?;
            println!("evaluation lock: {}", path.display());
            Ok(())
        }
        [command, capture, campaign, output_flag, output, name_flag, name]
            if command == "evaluate"
                && capture == "capture"
                && output_flag == "--output"
                && name_flag == "--name" =>
        {
            let path =
                capture_evaluation(campaign, output, name).map_err(|error| error.to_string())?;
            println!("evaluation: {}", path.display());
            Ok(())
        }
        [command, bundle, rest @ ..] if command == "query" => {
            let result: serde_json::Value = serde_json::from_slice(
                &std::fs::read(std::path::Path::new(bundle).join("campaign-result.json"))
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            let mut moment: Option<String> = None;
            let mut navigation: Option<&str> = None;
            let mut list = false;
            let mut format = "text";
            let mut service_filter: Option<String> = None;
            let mut index = 0;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--moment" => {
                        moment = Some(
                            rest.get(index + 1)
                                .ok_or(USAGE.to_owned())?
                                .clone(),
                        );
                        index += 2;
                    }
                    "--next" => {
                        navigation = Some("next");
                        index += 1;
                    }
                    "--previous" => {
                        navigation = Some("previous");
                        index += 1;
                    }
                    "--list" => {
                        list = true;
                        index += 1;
                    }
                    "--service" => {
                        service_filter = Some(
                            rest.get(index + 1)
                                .ok_or(USAGE.to_owned())?
                                .clone(),
                        );
                        index += 2;
                    }
                    "--format" => {
                        format = match rest.get(index + 1).map(String::as_str) {
                            Some("json") => "json",
                            Some("text") => "text",
                            _ => return Err(USAGE.to_owned().into()),
                        };
                        index += 2;
                    }
                    other => {
                        let _ = other;
                        return Err(USAGE.to_owned().into());
                    }
                }
            }
            if list {
                let summaries =
                    list_moments(&result, service_filter.as_deref())
                        .map_err(|error| error.to_string())?;
                if format == "json" {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&summaries)
                            .map_err(|error| error.to_string())?
                    );
                } else {
                    for summary in summaries {
                        println!(
                            "{}\t{}\t{}\t{}",
                            summary.moment, summary.run, summary.boundary, summary.service
                        );
                    }
                }
                return Ok(());
            }
            let Some(moment) = moment else {
                return Err(USAGE.to_owned().into());
            };
            let hit = match navigation {
                Some("next") => next_moment_in(&result, &moment),
                Some("previous") => previous_moment_in(&result, &moment),
                _ => find_moment(&result, &moment),
            }
            .map_err(|error| error.to_string())?;
            if format == "json" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&hit).map_err(|error| error.to_string())?
                );
                return Ok(());
            }
            println!("run: {}", hit.run);
            println!("boundary: {}", hit.boundary);
            println!("operation: {}", hit.operation);
            println!("service: {}", hit.service);
            println!("moment: {}", moment);
            println!("vtime_ns: {}", hit.vtime_ns);
            println!("input_sha256: {}", hit.input_sha256);
            for (service, excerpt) in &hit.excerpts {
                println!("excerpt {service}: {excerpt}");
            }
            if let Some(previous) = &hit.previous {
                println!("previous: {previous}");
            }
            if let Some(next) = &hit.next {
                println!("next: {next}");
            }
            Ok(())
        }
        [command] if command == "evaluate" => {
            let summary = evaluate("theseus-evaluation.toml").map_err(|error| error.to_string())?;
            print_evaluation(summary, "json")
        }
        [command, input] if command == "evaluate" => {
            let summary = evaluate(input).map_err(|error| error.to_string())?;
            print_evaluation(summary, "json")
        }
        [command, flag, format, input] if command == "evaluate" && flag == "--format" => {
            let summary = evaluate(input).map_err(|error| error.to_string())?;
            print_evaluation(summary, format)
        }
        [command, flag, moment, left, right]
            if command == "compare" && flag == "--at-moment" =>
        {
            let diff = boundary_at_moment(left, right, moment)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&diff).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        [command, left, right] if command == "compare" => {
            let comparison = compare_campaigns(left, right).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&comparison)
                    .map_err(|error| format!("cannot encode comparison: {error}"))?
            );
            Ok(())
        }
        [command, flag, format, left, right] if command == "compare" && flag == "--format" => {
            let comparison = compare_campaigns(left, right).map_err(|error| error.to_string())?;
            match format.as_str() {
                "json" => println!(
                    "{}",
                    serde_json::to_string_pretty(&comparison)
                        .map_err(|error| format!("cannot encode comparison: {error}"))?
                ),
                "markdown" => print!("{}", comparison.markdown()),
                _ => return Err("compare format must be json or markdown".to_owned()),
            }
            Ok(())
        }
        [command, flag, pointer, left, right] if command == "compare" && flag == "--query" => {
            let query = query_campaigns(left, right, pointer).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&query).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        [command, verify, index] if command == "evidence" && verify == "verify" => {
            let summary = verify_native_evidence(index).map_err(|error| error.to_string())?;
            println!(
                "verified native KVM evidence for {} at {}: {}",
                summary.architectures.join(", "),
                summary.source_commit,
                summary.runtime_tag
            );
            Ok(())
        }
        [command, subcommand, flag]
            if command == "coverage"
                && subcommand == "cargo"
                && (flag == "--help" || flag == "-h") =>
        {
            println!("{CARGO_COVERAGE_USAGE}");
            Ok(())
        }
        [command, subcommand, flag]
            if command == "coverage"
                && subcommand == "go"
                && (flag == "--help" || flag == "-h") =>
        {
            println!("{GO_COVERAGE_USAGE}");
            Ok(())
        }
        [command, flag] if command == "coverage" && (flag == "--help" || flag == "-h") => {
            println!("{CARGO_COVERAGE_USAGE}\n\n{GO_COVERAGE_USAGE}");
            Ok(())
        }
        [command, subcommand, rest @ ..] if command == "coverage" && subcommand == "cargo" => {
            let result = cargo_coverage(rest)?;
            println!(
                "built from {} Cargo package inputs; binary: {}; manifest: {}; symbols: {}",
                result.packages, result.binary, result.manifest, result.symbols
            );
            Ok(())
        }
        [command, subcommand, rest @ ..] if command == "coverage" && subcommand == "go" => {
            let result = go_coverage(rest)?;
            println!(
                "instrumented {} blocks across {} of {} Go packages; binary: {}; manifest: {}; symbols: {}",
                result.blocks,
                result.instrumented_packages,
                result.packages,
                result.binary,
                result.manifest,
                result.symbols
            );
            Ok(())
        }
        [command, ..] if command == "coverage" => {
            Err(format!("{CARGO_COVERAGE_USAGE}\n\n{GO_COVERAGE_USAGE}"))
        }
        [command, minimize, bundle, path_flag, path, output_flag, output]
            if command == "explore"
                && minimize == "--minimize"
                && path_flag == "--seed-path"
                && output_flag == "--output" =>
        {
            let result = minimize_exploration_path(bundle, seed_path(path)?, output)
                .map_err(|error| error.to_string())?;
            println!("minimized failing path: {}", result.display());
            Ok(())
        }
        [command, snapshot, bundle, path_flag, path, output_flag, output]
            if command == "explore"
                && snapshot == "--snapshot"
                && path_flag == "--seed-path"
                && output_flag == "--output" =>
        {
            let result = snapshot_exploration_path(bundle, seed_path(path)?, output)
                .map_err(|error| error.to_string())?;
            println!("exported snapshot: {}", result.display());
            Ok(())
        }
        [command, snapshot, bundle, path_flag, path]
            if command == "explore" && snapshot == "--snapshot" && path_flag == "--seed-path" =>
        {
            let result =
                snapshot_exploration_path(bundle, seed_path(path)?, format!("{bundle}-snapshot"))
                    .map_err(|error| error.to_string())?;
            println!("exported snapshot: {}", result.display());
            Ok(())
        }
        [command, minimize, bundle, path_flag, path]
            if command == "explore" && minimize == "--minimize" && path_flag == "--seed-path" =>
        {
            let result =
                minimize_exploration_path(bundle, seed_path(path)?, format!("{bundle}-minimized"))
                    .map_err(|error| error.to_string())?;
            println!("minimized failing path: {}", result.display());
            Ok(())
        }
        [command, replay, bundle, path_flag, path, output_flag, output]
            if command == "explore"
                && replay == "--replay"
                && path_flag == "--seed-path"
                && output_flag == "--output" =>
        {
            let result = replay_exploration_path(bundle, seed_path(path)?, output)
                .map_err(|error| error.to_string())?;
            println!("exploration path replay passed: {}", result.display());
            Ok(())
        }
        [command, replay, bundle, path_flag, path]
            if command == "explore" && replay == "--replay" && path_flag == "--seed-path" =>
        {
            let result =
                replay_exploration_path(bundle, seed_path(path)?, format!("{bundle}-path-replay"))
                    .map_err(|error| error.to_string())?;
            println!("exploration path replay passed: {}", result.display());
            Ok(())
        }
        [command, flag, bundle, output_flag, output]
            if command == "explore" && flag == "--replay" && output_flag == "--output" =>
        {
            let result = replay_exploration(bundle, output).map_err(|error| error.to_string())?;
            println!("exploration replay passed: {}", result.display());
            Ok(())
        }
        [command, flag, bundle] if command == "explore" && flag == "--replay" => {
            let result = replay_exploration(bundle, format!("{bundle}-replay"))
                .map_err(|error| error.to_string())?;
            println!("exploration replay passed: {}", result.display());
            Ok(())
        }
        [command, flag, output, rest @ ..] if command == "explore" && flag == "--output" => {
            let manifest = manifest_path(rest)?;
            let result = explore(&manifest, output).map_err(|error| error.to_string())?;
            println!("exploration passed: {}", result.display());
            Ok(())
        }
        [command, rest @ ..] if command == "explore" => {
            let manifest = manifest_path(rest)?;
            let output = manifest
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("theseus-exploration");
            let result = explore(&manifest, output).map_err(|error| error.to_string())?;
            println!("exploration passed: {}", result.display());
            Ok(())
        }
        [command, subcommand, bundle] if command == "compose" && subcommand == "verify" => {
            let summary = verify_topology_bundle(bundle)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        [command, subcommand, rest @ ..] if command == "compose" && subcommand == "validate" => {
            let path = compose_path(rest)?;
            let plan = load_compose_plan(&path).map_err(|error| error.to_string())?;
            println!("valid: {} ({} services)", plan.compose, plan.services.len());
            Ok(())
        }
        [command, subcommand, rest @ ..] if command == "compose" && subcommand == "plan" => {
            let path = compose_path(rest)?;
            let plan = load_compose_plan(&path).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&plan)
                    .map_err(|error| format!("could not serialize compose plan: {error}"))?
            );
            Ok(())
        }
        [command, subcommand, flag, output, rest @ ..]
            if command == "compose" && subcommand == "test" && flag == "--output" =>
        {
            let compose = compose_path(rest)?;
            let result = test_compose(&compose, output).map_err(|error| error.to_string())?;
            println!("passed: {}", result.display());
            Ok(())
        }
        [command, subcommand, rest @ ..] if command == "compose" && subcommand == "test" => {
            let compose = compose_path(rest)?;
            let output = compose
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("theseus-compose-replay");
            let result = test_compose(&compose, output).map_err(|error| error.to_string())?;
            println!("passed: {}", result.display());
            Ok(())
        }
        [command, subcommand, minimize, bundle, expect, property, output_flag, output]
            if command == "compose"
                && subcommand == "explore"
                && minimize == "--minimize"
                && expect == "--expect-counterexample"
                && output_flag == "--output" =>
        {
            let result = minimize_compose_campaign_expect_counterexample(bundle, output, property)
                .map_err(|error| error.to_string())?;
            println!("minimized campaign counterexample: {}", result.display());
            Ok(())
        }
        [command, subcommand, minimize, bundle, expect, property]
            if command == "compose"
                && subcommand == "explore"
                && minimize == "--minimize"
                && expect == "--expect-counterexample" =>
        {
            let result = minimize_compose_campaign_expect_counterexample(
                bundle,
                format!("{bundle}-minimized"),
                property,
            )
            .map_err(|error| error.to_string())?;
            println!("minimized campaign counterexample: {}", result.display());
            Ok(())
        }
        [command, subcommand, minimize, bundle, output_flag, output]
            if command == "compose"
                && subcommand == "explore"
                && minimize == "--minimize"
                && output_flag == "--output" =>
        {
            let result =
                minimize_compose_campaign(bundle, output).map_err(|error| error.to_string())?;
            println!("minimized campaign counterexample: {}", result.display());
            Ok(())
        }
        [command, subcommand, minimize, bundle]
            if command == "compose" && subcommand == "explore" && minimize == "--minimize" =>
        {
            let result = minimize_compose_campaign(bundle, format!("{bundle}-minimized"))
                .map_err(|error| error.to_string())?;
            println!("minimized campaign counterexample: {}", result.display());
            Ok(())
        }
        [command, subcommand, expect, property, output_flag, output, rest @ ..]
            if command == "compose"
                && subcommand == "explore"
                && expect == "--expect-counterexample"
                && output_flag == "--output" =>
        {
            let (compose, (max_runs, guidance)) = compose_explore_overrides(rest)?;
            let result = explore_compose_expect_counterexample_with(
                &compose,
                output,
                property,
                max_runs,
                guidance,
            )
            .map_err(|error| error.to_string())?;
            println!("counterexample retained: {}", result.display());
            Ok(())
        }
        [command, subcommand, expect, property, rest @ ..]
            if command == "compose"
                && subcommand == "explore"
                && expect == "--expect-counterexample" =>
        {
            let (compose, (max_runs, guidance)) = compose_explore_overrides(rest)?;
            let output = compose
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("theseus-compose-campaign");
            let result = explore_compose_expect_counterexample_with(
                &compose,
                output,
                property,
                max_runs,
                guidance,
            )
            .map_err(|error| error.to_string())?;
            println!("counterexample retained: {}", result.display());
            Ok(())
        }
        [command, subcommand, flag, output, rest @ ..]
            if command == "compose" && subcommand == "explore" && flag == "--output" =>
        {
            let (compose, overrides) = compose_explore_overrides(rest)?;
            let result =
                explore_compose_with(&compose, output, overrides.0, overrides.1)
                    .map_err(|error| error.to_string())?;
            println!("campaign passed: {}", result.display());
            Ok(())
        }
        [command, subcommand, rest @ ..] if command == "compose" && subcommand == "explore" => {
            let (compose, (max_runs, guidance)) = compose_explore_overrides(rest)?;
            let output = compose
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("theseus-compose-campaign");
            let result = explore_compose_with(&compose, output, max_runs, guidance)
                .map_err(|error| error.to_string())?;
            println!("campaign passed: {}", result.display());
            Ok(())
        }
        [command, subcommand, bundle, output_flag, output]
            if command == "compose" && subcommand == "replay" && output_flag == "--output" =>
        {
            let result = replay_compose(bundle, output).map_err(|error| error.to_string())?;
            println!("topology replay passed: {}", result.display());
            Ok(())
        }
        [command, subcommand, bundle] if command == "compose" && subcommand == "replay" => {
            let result = replay_compose(bundle, format!("{bundle}-replay"))
                .map_err(|error| error.to_string())?;
            println!("topology replay passed: {}", result.display());
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    }
}

/// Parse `--max-runs N` and `--guidance MODE` exploration overrides from the
/// remaining arguments, leaving the manifest path in place.
fn compose_explore_overrides(
    args: &[String],
) -> Result<(PathBuf, (Option<u16>, Option<CampaignGuidance>)), String> {
    let mut max_runs = None;
    let mut guidance = None;
    let mut rest: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--max-runs" => {
                let value = args.get(index + 1).ok_or(USAGE.to_owned())?;
                max_runs = Some(value.parse::<u16>().map_err(|_| USAGE.to_owned())?);
                if max_runs == Some(0) {
                    return Err(USAGE.to_owned());
                }
                index += 2;
            }
            "--guidance" => {
                let value = args.get(index + 1).ok_or(USAGE.to_owned())?;
                guidance = Some(parse_guidance(value)?);
                index += 2;
            }
            other => {
                rest.push(other.to_owned());
                index += 1;
            }
        }
    }
    Ok((compose_path(&rest)?, (max_runs, guidance)))
}

fn parse_guidance(value: &str) -> Result<CampaignGuidance, String> {
    match value {
        "coverage" => Ok(CampaignGuidance::Coverage),
        "adaptive" => Ok(CampaignGuidance::Adaptive),
        "posterior" => Ok(CampaignGuidance::Posterior),
        "property" => Ok(CampaignGuidance::Property),
        "unified" => Ok(CampaignGuidance::Unified),
        _ => Err(USAGE.to_owned()),
    }
}

fn compose_path(args: &[String]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(PathBuf::from("compose.yaml")),
        [path] => Ok(PathBuf::from(path)),
        _ => Err(format!("{USAGE}\n\n{COMPOSE_USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_root_to_node_seed_path() {
        assert_eq!(seed_path("42,7,9").unwrap(), vec![42, 7, 9]);
        assert!(seed_path("42,,9").is_err());
        assert!(seed_path("").is_err());
    }
}

fn main() -> ExitCode {
    if theseus_cli::is_cargo_coverage_wrapper() {
        return match cargo_coverage_rustc_wrapper(&env::args_os().skip(1).collect::<Vec<_>>()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("theseus: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    fn overrides(args: &[&str]) -> Result<(PathBuf, (Option<u16>, Option<CampaignGuidance>)), String> {
        compose_explore_overrides(
            &args
                .iter()
                .map(|argument| argument.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn explore_overrides_parse_budget_guidance_and_path() {
        let (path, (max_runs, guidance)) =
            overrides(&["--max-runs", "64", "--guidance", "unified", "compose.yaml"]).unwrap();
        assert_eq!(path, PathBuf::from("compose.yaml"));
        assert_eq!(max_runs, Some(64));
        assert!(matches!(guidance, Some(CampaignGuidance::Unified)));

        let (path, (max_runs, guidance)) =
            overrides(&["--guidance", "coverage", "--max-runs", "8", "work/compose.yaml"]).unwrap();
        assert_eq!(path, PathBuf::from("work/compose.yaml"));
        assert_eq!(max_runs, Some(8));
        assert!(matches!(guidance, Some(CampaignGuidance::Coverage)));

        // Interleaving keeps the manifest path in place.
        let (path, _) = overrides(&["compose.yaml", "--max-runs", "3"]).unwrap();
        assert_eq!(path, PathBuf::from("compose.yaml"));

        // Every documented guidance mode parses to its enum variant.
        for (name, expected) in [
            ("coverage", CampaignGuidance::Coverage),
            ("adaptive", CampaignGuidance::Adaptive),
            ("posterior", CampaignGuidance::Posterior),
            ("property", CampaignGuidance::Property),
            ("unified", CampaignGuidance::Unified),
        ] {
            let (_, (_, guidance)) =
                overrides(&["--guidance", name, "compose.yaml"]).unwrap();
            assert!(matches!(guidance, Some(mode) if mode == expected), "{name}");
        }
    }

    #[test]
    fn explore_overrides_pass_the_manifest_through_untouched() {
        let (path, (max_runs, guidance)) = overrides(&["compose.yaml"]).unwrap();
        assert_eq!(path, PathBuf::from("compose.yaml"));
        assert_eq!(max_runs, None);
        assert_eq!(guidance, None);
    }

    #[test]
    fn explore_overrides_reject_malformed_values() {
        assert!(overrides(&["--max-runs", "0", "compose.yaml"]).is_err());
        assert!(overrides(&["--max-runs", "many", "compose.yaml"]).is_err());
        assert!(overrides(&["--max-runs"]).is_err());
        assert!(overrides(&["--guidance", "sometimes", "compose.yaml"]).is_err());
        assert!(overrides(&["--guidance"]).is_err());
    }
}
