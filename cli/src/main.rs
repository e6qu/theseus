// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use theseus_cli::{explore, load_compose_plan, load_plan, minimize_exploration_path, replay, replay_compose, replay_exploration, replay_exploration_path, report, test, test_compose};

const USAGE: &str = "Usage:
  theseus validate [theseus.toml]
  theseus test --dry-run [theseus.toml]
  theseus test [--output replay-dir] [theseus.toml]
  theseus replay replay-dir
  theseus explore [--output exploration-dir] [theseus.toml]
  theseus explore --replay exploration-dir [--seed-path seed,...] [--output exploration-dir]
  theseus explore --minimize exploration-dir --seed-path seed,... [--output exploration-dir]
  theseus report [--output report-dir] result-dir
  theseus compose validate [compose.yaml]
  theseus compose plan [compose.yaml]
  theseus compose test [--output replay-dir] [compose.yaml]
  theseus compose replay replay-dir [--output replay-dir]

The manifest path defaults to ./theseus.toml. Relative artifact paths are
resolved from the directory containing that manifest.";

const COMPOSE_USAGE: &str = "Compose accepts a small Theseus-only subset. Each service must set
x-theseus.manifest to a relative theseus.toml path; services join named networks.
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
        [command, minimize, bundle, path_flag, path]
            if command == "explore" && minimize == "--minimize" && path_flag == "--seed-path" =>
        {
            let result = minimize_exploration_path(
                bundle,
                seed_path(path)?,
                format!("{bundle}-minimized"),
            )
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
            let result = replay_exploration_path(
                bundle,
                seed_path(path)?,
                format!("{bundle}-path-replay"),
            )
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
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("theseus: {error}");
            ExitCode::FAILURE
        }
    }
}
