use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str =
    "Usage: theseus-image flatten image.tar --output initramfs.cpio [--service service.json]";

fn run(args: Vec<String>) -> Result<(), String> {
    match args.as_slice() {
        [command] if command == "--help" || command == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => flatten(args),
    }
}

fn flatten(args: Vec<String>) -> Result<(), String> {
    let [command, image, output_flag, output, rest @ ..] = args.as_slice() else {
        return Err(USAGE.to_owned());
    };
    if command != "flatten" || output_flag != "--output" {
        return Err(USAGE.to_owned());
    }
    let service = match rest {
        [] => None,
        [flag, path] if flag == "--service" => {
            let bytes = fs::read(path)
                .map_err(|error| format!("cannot read service contract {path}: {error}"))?;
            Some(
                serde_json::from_slice(&bytes)
                    .map_err(|error| format!("cannot parse service contract {path}: {error}"))?,
            )
        }
        _ => return Err(USAGE.to_owned()),
    };
    let image = fs::read(image).map_err(|error| format!("cannot read image archive: {error}"))?;
    let (initramfs, contract) =
        theseus_orchestrator::oci::flatten_with_service(&image, service.as_ref())
            .map_err(|error| format!("cannot flatten image archive: {error}"))?;
    let output = PathBuf::from(output);
    fs::write(&output, initramfs)
        .map_err(|error| format!("cannot write {}: {error}", output.display()))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&contract).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("theseus-image: {error}");
            ExitCode::FAILURE
        }
    }
}
