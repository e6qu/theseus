use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Usage: theseus-image flatten image.tar --output initramfs.cpio [--service service.json] [--network network.json] [--environment environment.json] [--launch launch.json] [--configs configs.json]";

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
    let mut service = None;
    let mut network = None;
    let mut environment = None;
    let mut launch = None;
    let mut configs: Option<Vec<theseus_orchestrator::oci::ContainerConfig>> = None;
    let mut options = rest.iter();
    while let Some(flag) = options.next() {
        let Some(path) = options.next() else {
            return Err(USAGE.to_owned());
        };
        match flag.as_str() {
            "--service" if service.is_none() => {
                let bytes = fs::read(path)
                    .map_err(|error| format!("cannot read service contract {path}: {error}"))?;
                service =
                    Some(serde_json::from_slice(&bytes).map_err(|error| {
                        format!("cannot parse service contract {path}: {error}")
                    })?);
            }
            "--network" if network.is_none() => {
                let bytes = fs::read(path)
                    .map_err(|error| format!("cannot read network contract {path}: {error}"))?;
                network =
                    Some(serde_json::from_slice(&bytes).map_err(|error| {
                        format!("cannot parse network contract {path}: {error}")
                    })?);
            }
            "--environment" if environment.is_none() => {
                let bytes = fs::read(path)
                    .map_err(|error| format!("cannot read environment contract {path}: {error}"))?;
                environment = Some(serde_json::from_slice(&bytes).map_err(|error| {
                    format!("cannot parse environment contract {path}: {error}")
                })?);
            }
            "--launch" if launch.is_none() => {
                let bytes = fs::read(path)
                    .map_err(|error| format!("cannot read launch contract {path}: {error}"))?;
                launch =
                    Some(serde_json::from_slice(&bytes).map_err(|error| {
                        format!("cannot parse launch contract {path}: {error}")
                    })?);
            }
            "--configs" if configs.is_none() => {
                let bytes = fs::read(path)
                    .map_err(|error| format!("cannot read config contract {path}: {error}"))?;
                configs =
                    Some(serde_json::from_slice(&bytes).map_err(|error| {
                        format!("cannot parse config contract {path}: {error}")
                    })?);
            }
            _ => return Err(USAGE.to_owned()),
        }
    }
    let image = fs::read(image).map_err(|error| format!("cannot read image archive: {error}"))?;
    let (initramfs, contract) = theseus_orchestrator::oci::flatten_with_contracts(
        &image,
        service.as_ref(),
        network.as_ref(),
        environment.as_ref(),
        launch.as_ref(),
        configs.as_deref(),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn accepts_image_contracts_in_either_order() {
        let directory = std::env::temp_dir().join(format!(
            "theseus-image-options-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let service = directory.join("service.json");
        let network = directory.join("network.json");
        let environment = directory.join("environment.json");
        let launch = directory.join("launch.json");
        fs::write(&service, "{}").unwrap();
        fs::write(&network, r#"{"interfaces":[],"hosts":{}}"#).unwrap();
        fs::write(&environment, r#"{"MODE":"campaign"}"#).unwrap();
        fs::write(&launch, r#"{"command":["--serve"],"working_dir":"/srv"}"#).unwrap();
        let error = flatten(vec![
            "flatten".to_owned(),
            directory.join("missing.tar").display().to_string(),
            "--output".to_owned(),
            directory.join("initramfs.cpio").display().to_string(),
            "--network".to_owned(),
            network.display().to_string(),
            "--service".to_owned(),
            service.display().to_string(),
            "--environment".to_owned(),
            environment.display().to_string(),
            "--launch".to_owned(),
            launch.display().to_string(),
        ])
        .unwrap_err();
        assert!(error.contains("cannot read image archive"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_duplicate_network_contracts() {
        let directory = std::env::temp_dir().join(format!(
            "theseus-image-duplicate-options-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let one = directory.join("one.json");
        let two = directory.join("two.json");
        fs::write(&one, r#"{"interfaces":[],"hosts":{}}"#).unwrap();
        fs::write(&two, r#"{"interfaces":[],"hosts":{}}"#).unwrap();
        let error = flatten(vec![
            "flatten".to_owned(),
            "image.tar".to_owned(),
            "--output".to_owned(),
            "initramfs.cpio".to_owned(),
            "--network".to_owned(),
            one.display().to_string(),
            "--network".to_owned(),
            two.display().to_string(),
        ])
        .unwrap_err();
        assert_eq!(error, USAGE);
        fs::remove_dir_all(directory).unwrap();
    }
}
