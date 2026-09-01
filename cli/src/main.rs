// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use theseus_cli::{explore, load_compose_plan, load_plan, replay, report, test, test_compose};

const USAGE: &str = "Usage:
  theseus validate [theseus.toml]
  theseus test --dry-run [theseus.toml]
  theseus test [--output replay-dir] [theseus.toml]
  theseus replay replay-dir
  theseus explore [--output exploration-dir] [theseus.toml]
  theseus report [--output report-dir] result-dir
  theseus compose validate [compose.yaml]
  theseus compose plan [compose.yaml]
  theseus compose test [--output replay-dir] [compose.yaml]

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

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("theseus: {error}");
            ExitCode::FAILURE
        }
    }
}
