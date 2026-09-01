// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use theseus_cli::load_plan;

const USAGE: &str = "Usage:
  theseus validate [theseus.toml]
  theseus test --dry-run [theseus.toml]

The manifest path defaults to ./theseus.toml. Relative artifact paths are
resolved from the directory containing that manifest.";

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
        [command, ..] if command == "test" => Err(
            "`theseus test` will execute one timeline in P6.2; use `theseus test --dry-run` in P6.1"
                .to_owned(),
        ),
        _ => Err(USAGE.to_owned()),
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
