use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Usage: theseus-image flatten image.tar --output initramfs.cpio";

fn run(args: Vec<String>) -> Result<(), String> {
    match args.as_slice() {
        [command] if command == "--help" || command == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        [command, image, output_flag, output]
            if command == "flatten" && output_flag == "--output" =>
        {
            let image =
                fs::read(image).map_err(|error| format!("cannot read image archive: {error}"))?;
            let (initramfs, contract) = theseus_orchestrator::oci::flatten(&image)
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
        _ => Err(USAGE.to_owned()),
    }
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
