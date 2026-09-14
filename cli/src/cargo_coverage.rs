// Copyright 2026 Adrian Mârza and contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Build one Cargo binary with LLVM edge coverage across its static Rust
//! dependency graph. The same executable acts as Cargo's rustc wrapper so the
//! user does not have to install or coordinate another helper.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const WRAPPER: &str = "THESEUS_COVERAGE_RUSTC_WRAPPER";
const BUILD_SHA256: &str = "THESEUS_COVERAGE_BUILD_SHA256";
const PRIMARY_CRATE: &str = "THESEUS_COVERAGE_PRIMARY_CRATE";
const RUNTIME_OBJECT: &str = "THESEUS_COVERAGE_RUNTIME_OBJECT";
const WORKSPACE_ROOT: &str = "THESEUS_COVERAGE_WORKSPACE_ROOT";
const RUNTIME_SOURCE: &[u8] = include_bytes!("../../instrumentation/llvm/theseus_coverage.c");
const MAXIMUM_EDGES: u32 = 65_535;

pub const CARGO_COVERAGE_USAGE: &str = "Usage:
  theseus coverage cargo --process NAME --module NAME --bin NAME --symbols DIR --output FILE
      [--manifest-path Cargo.toml] [--package NAME] [--release] [--locked]
      [--offline] [--no-default-features] [--features FEATURES] [--target-dir DIR]";

#[derive(Debug)]
struct Options {
    process: String,
    module: String,
    binary: String,
    package: Option<String>,
    manifest_path: PathBuf,
    symbols: PathBuf,
    output: PathBuf,
    target_dir: Option<PathBuf>,
    release: bool,
    locked: bool,
    offline: bool,
    no_default_features: bool,
    features: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CargoCoverageOutput {
    pub binary: String,
    pub manifest: String,
    pub symbols: String,
    pub build_sha256: String,
    pub packages: usize,
}

#[derive(Debug, Clone)]
struct Package {
    id: String,
    name: String,
    version: String,
    source: Option<String>,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

#[derive(Debug, Clone)]
struct Target {
    name: String,
    kinds: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CoverageManifest<'a> {
    format: &'static str,
    coverage: &'static str,
    maximum_edges: u32,
    language: &'static str,
    process: &'a str,
    module: &'a str,
    build_sha256: &'a str,
    gnu_build_id: &'a str,
    compiler: &'a str,
    target: &'a str,
    symbols: &'a str,
    cargo: CargoManifest<'a>,
    sources: &'a [PackageDigest],
}

#[derive(Debug, Serialize)]
struct CargoManifest<'a> {
    package: &'a str,
    binary: &'a str,
    profile: &'static str,
    packages: usize,
    workspace_sha256: &'a str,
    rust_target_dependencies_instrumented: bool,
    host_build_targets_instrumented: bool,
    dynamic_rust_targets_instrumented: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PackageDigest {
    name: String,
    version: String,
    source_sha256: String,
}

pub fn cargo_coverage(args: &[String]) -> Result<CargoCoverageOutput, String> {
    if !cfg!(target_os = "linux") {
        return Err("Cargo coverage builds require Linux".to_owned());
    }
    let options = parse_options(args)?;
    require_command("cargo")?;
    require_command("rustc")?;
    require_command("clang")?;
    require_command("readelf")?;

    let manifest_path = fs::canonicalize(&options.manifest_path).map_err(|error| {
        format!(
            "cannot resolve Cargo manifest {}: {error}",
            options.manifest_path.display()
        )
    })?;
    if manifest_path.file_name() != Some(OsStr::new("Cargo.toml")) {
        return Err("--manifest-path must name Cargo.toml".to_owned());
    }
    let metadata = cargo_metadata(&options, &manifest_path)?;
    let packages = metadata_packages(&metadata)?;
    let workspace_members = metadata_workspace_members(&metadata)?;
    let selected = select_package(
        &packages,
        &workspace_members,
        options.package.as_deref(),
        &options.binary,
    )?;
    let package_digests = dependency_digests(
        &metadata,
        &packages,
        &selected.id,
        &options.output,
        &options.symbols,
        options.target_dir.as_deref(),
    )?;
    let workspace_sha256 = workspace_configuration_sha256(&metadata)?;
    let workspace_root = metadata["workspace_root"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata has no workspace root".to_owned())?;

    let rustc_version = command_text("rustc", &["--version", "--verbose"])?;
    let cargo_version = command_text("cargo", &["--version", "--verbose"])?;
    let target = rustc_version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| "rustc did not report its host target".to_owned())?
        .to_owned();
    let frontend_sha256 = file_sha256(
        &env::current_exe().map_err(|error| format!("cannot locate theseus: {error}"))?,
    )?;
    let runtime_sha256 = format!("{:x}", Sha256::digest(RUNTIME_SOURCE));
    let ledger = serde_json::json!({
        "format": "theseus-cargo-coverage-input-v1",
        "process": options.process,
        "module": options.module,
        "package": selected.name,
        "binary": options.binary,
        "profile": if options.release { "release" } else { "dev" },
        "no_default_features": options.no_default_features,
        "features": options.features,
        "rustc": rustc_version,
        "cargo": cargo_version,
        "target": target,
        "frontend_sha256": frontend_sha256,
        "runtime_sha256": runtime_sha256,
        "packages": package_digests,
        "workspace_sha256": workspace_sha256,
    });
    let build_sha256 = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&ledger)
                .map_err(|error| format!("cannot encode Cargo build inputs: {error}"))?
        )
    );

    let target_base = options.target_dir.clone().unwrap_or_else(|| {
        manifest_path
            .parent()
            .expect("Cargo.toml has a parent")
            .join("target/theseus-coverage")
    });
    let target_dir = target_base.join(&build_sha256[..16]);
    fs::create_dir_all(&target_dir)
        .map_err(|error| format!("cannot create {}: {error}", target_dir.display()))?;
    let work = target_dir.join(format!(".theseus-runtime-{}", std::process::id()));
    fs::create_dir_all(&work)
        .map_err(|error| format!("cannot create coverage work directory: {error}"))?;
    let runtime_source = work.join("theseus_coverage.c");
    let runtime_object = work.join("theseus_coverage.o");
    let build = (|| {
        File::create(&runtime_source)
            .and_then(|mut file| file.write_all(RUNTIME_SOURCE))
            .map_err(|error| format!("cannot write coverage runtime: {error}"))?;
        compile_runtime(
            &runtime_source,
            &runtime_object,
            &options.process,
            &options.module,
            &build_sha256,
            &target,
        )?;
        build_cargo_binary(
            &options,
            &manifest_path,
            selected,
            &target,
            &target_dir,
            &runtime_object,
            &build_sha256,
            &workspace_root,
        )
    })();
    let _ = fs::remove_dir_all(&work);
    let executable = build?;
    let output = absolute_output(&options.output)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    if executable != output {
        fs::copy(&executable, &output).map_err(|error| {
            format!(
                "cannot copy {} to {}: {error}",
                executable.display(),
                output.display()
            )
        })?;
    }
    let bytes =
        fs::read(&output).map_err(|error| format!("cannot read {}: {error}", output.display()))?;
    if !bytes
        .windows(build_sha256.len())
        .any(|window| window == build_sha256.as_bytes())
    {
        return Err("instrumented Cargo binary does not contain its build identity".to_owned());
    }
    let gnu_build_id = gnu_build_id(&output)?;
    fs::create_dir_all(&options.symbols)
        .map_err(|error| format!("cannot create {}: {error}", options.symbols.display()))?;
    let symbol_name = format!("{}-{build_sha256}.debug", options.module);
    let symbol_path = absolute_output(&options.symbols.join(&symbol_name))?;
    fs::copy(&output, &symbol_path)
        .map_err(|error| format!("cannot preserve {}: {error}", symbol_path.display()))?;

    let manifest_path = appended_path(&output, ".theseus-coverage.json");
    let manifest = CoverageManifest {
        format: "theseus-llvm-coverage-build-v1",
        coverage: "edges",
        maximum_edges: MAXIMUM_EDGES,
        language: "rust",
        process: &options.process,
        module: &options.module,
        build_sha256: &build_sha256,
        gnu_build_id: &gnu_build_id,
        compiler: rustc_version.trim(),
        target: &target,
        symbols: &symbol_name,
        cargo: CargoManifest {
            package: &selected.name,
            binary: &options.binary,
            profile: if options.release { "release" } else { "dev" },
            packages: package_digests.len(),
            workspace_sha256: &workspace_sha256,
            rust_target_dependencies_instrumented: true,
            host_build_targets_instrumented: false,
            dynamic_rust_targets_instrumented: false,
        },
        sources: &package_digests,
    };
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("cannot encode coverage manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot write {}: {error}", manifest_path.display()))?;

    Ok(CargoCoverageOutput {
        binary: output.display().to_string(),
        manifest: manifest_path.display().to_string(),
        symbols: symbol_path.display().to_string(),
        build_sha256,
        packages: package_digests.len(),
    })
}

/// Entry point used when Cargo invokes the `theseus` executable as
/// `RUSTC_WRAPPER`. Compiler probes and host build dependencies pass through;
/// target Rust libraries and the selected binary receive the same LLVM edge
/// instrumentation, while only the final binary links the module runtime.
pub fn cargo_coverage_rustc_wrapper(args: &[OsString]) -> Result<(), String> {
    let (rustc, rustc_args) = args
        .split_first()
        .ok_or_else(|| "coverage rustc wrapper received no compiler".to_owned())?;
    let mut command = Command::new(rustc);
    command.args(rustc_args);
    if wrapper_should_instrument(rustc_args) {
        let build_sha256 = env::var(BUILD_SHA256)
            .map_err(|_| "coverage rustc wrapper has no build identity".to_owned())?;
        if build_sha256.len() != 64 || !build_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("coverage rustc wrapper has an invalid build identity".to_owned());
        }
        command.args([
            "-Ccodegen-units=1",
            "-Cdebuginfo=2",
            "-Cstrip=none",
            "-Cpasses=sancov-module",
            "-Cllvm-args=-sanitizer-coverage-level=3",
            "-Cllvm-args=-sanitizer-coverage-trace-pc-guard",
        ]);
        let workspace = env::var_os(WORKSPACE_ROOT)
            .ok_or_else(|| "coverage rustc wrapper has no workspace root".to_owned())?;
        command.arg(format!(
            "--remap-path-prefix={}=.",
            Path::new(&workspace).display()
        ));
        let crate_name = rustc_arg_value(rustc_args, "--crate-name").unwrap_or_default();
        if env::var(PRIMARY_CRATE).ok().as_deref() == Some(crate_name.as_str())
            && rustc_crate_types(rustc_args).contains("bin")
        {
            let runtime = env::var_os(RUNTIME_OBJECT)
                .ok_or_else(|| "coverage rustc wrapper has no runtime object".to_owned())?;
            command.arg("-Crelocation-model=pie");
            command.arg("-Clink-arg=-Wl,--build-id=sha1");
            command.arg(format!("-Clink-arg={}", Path::new(&runtime).display()));
        }
    }
    let status = command
        .status()
        .map_err(|error| format!("cannot start rustc: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("rustc exited with {status}"))
    }
}

pub fn is_cargo_coverage_wrapper() -> bool {
    env::var_os(WRAPPER).is_some()
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut process = None;
    let mut module = None;
    let mut binary = None;
    let mut package = None;
    let mut manifest_path = PathBuf::from("Cargo.toml");
    let mut symbols = None;
    let mut output = None;
    let mut target_dir = None;
    let mut release = false;
    let mut locked = false;
    let mut offline = false;
    let mut no_default_features = false;
    let mut features = None;
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        let value = |index: &mut usize| -> Result<String, String> {
            *index += 1;
            args.get(*index)
                .cloned()
                .ok_or_else(|| CARGO_COVERAGE_USAGE.to_owned())
        };
        match flag.as_str() {
            "--process" => process = Some(value(&mut index)?),
            "--module" => module = Some(value(&mut index)?),
            "--bin" => binary = Some(value(&mut index)?),
            "--package" => package = Some(value(&mut index)?),
            "--manifest-path" => manifest_path = PathBuf::from(value(&mut index)?),
            "--symbols" => symbols = Some(PathBuf::from(value(&mut index)?)),
            "--output" => output = Some(PathBuf::from(value(&mut index)?)),
            "--target-dir" => target_dir = Some(PathBuf::from(value(&mut index)?)),
            "--features" => features = Some(value(&mut index)?),
            "--release" => release = true,
            "--locked" => locked = true,
            "--offline" => offline = true,
            "--no-default-features" => no_default_features = true,
            _ => return Err(CARGO_COVERAGE_USAGE.to_owned()),
        }
        index += 1;
    }
    let process = process.ok_or_else(|| CARGO_COVERAGE_USAGE.to_owned())?;
    let module = module.ok_or_else(|| CARGO_COVERAGE_USAGE.to_owned())?;
    let binary = binary.ok_or_else(|| CARGO_COVERAGE_USAGE.to_owned())?;
    validate_identity("process", &process)?;
    validate_identity("module", &module)?;
    validate_cargo_name("binary", &binary)?;
    if let Some(package) = &package {
        validate_cargo_name("package", package)?;
    }
    if features.as_deref().is_some_and(str::is_empty) {
        return Err("--features must not be empty".to_owned());
    }
    Ok(Options {
        process,
        module,
        binary,
        package,
        manifest_path,
        symbols: symbols.ok_or_else(|| CARGO_COVERAGE_USAGE.to_owned())?,
        output: output.ok_or_else(|| CARGO_COVERAGE_USAGE.to_owned())?,
        target_dir,
        release,
        locked,
        offline,
        no_default_features,
        features,
    })
}

fn validate_identity(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!("coverage {field} {value:?} is invalid"));
    }
    Ok(())
}

fn validate_cargo_name(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(format!("Cargo {field} {value:?} is invalid"));
    }
    Ok(())
}

fn cargo_metadata(options: &Options, manifest: &Path) -> Result<Value, String> {
    let rustc = command_text("rustc", &["-vV"])?;
    let target = rustc
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| "rustc did not report its host target".to_owned())?;
    let mut command = Command::new("cargo");
    command.args(["metadata", "--format-version", "1", "--manifest-path"]);
    command.arg(manifest);
    command.args(["--filter-platform", target]);
    cargo_feature_args(&mut command, options);
    let output = command
        .output()
        .map_err(|error| format!("cannot start cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(command_failure("cargo metadata", &output));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata returned invalid JSON: {error}"))
}

fn metadata_packages(metadata: &Value) -> Result<BTreeMap<String, Package>, String> {
    let entries = metadata["packages"]
        .as_array()
        .ok_or_else(|| "cargo metadata has no package list".to_owned())?;
    let mut result = BTreeMap::new();
    for entry in entries {
        let string = |field: &str| {
            entry[field]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("cargo package has no {field}"))
        };
        let id = string("id")?;
        let targets = entry["targets"]
            .as_array()
            .ok_or_else(|| "cargo package has no targets".to_owned())?
            .iter()
            .map(|target| {
                Ok(Target {
                    name: target["name"]
                        .as_str()
                        .ok_or_else(|| "Cargo target has no name".to_owned())?
                        .to_owned(),
                    kinds: target["kind"]
                        .as_array()
                        .ok_or_else(|| "Cargo target has no kind".to_owned())?
                        .iter()
                        .map(|kind| {
                            kind.as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| "Cargo target kind is not text".to_owned())
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        result.insert(
            id.clone(),
            Package {
                id,
                name: string("name")?,
                version: string("version")?,
                source: entry["source"].as_str().map(str::to_owned),
                manifest_path: PathBuf::from(string("manifest_path")?),
                targets,
            },
        );
    }
    Ok(result)
}

fn select_package<'a>(
    packages: &'a BTreeMap<String, Package>,
    workspace_members: &BTreeSet<String>,
    requested_package: Option<&str>,
    binary: &str,
) -> Result<&'a Package, String> {
    let matches = packages
        .values()
        .filter(|package| workspace_members.contains(&package.id))
        .filter(|package| requested_package.is_none_or(|name| package.name == name))
        .filter(|package| {
            package.targets.iter().any(|target| {
                target.name == binary && target.kinds.iter().any(|kind| kind == "bin")
            })
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [package] => Ok(package),
        [] => Err(format!("Cargo binary {binary:?} was not found")),
        _ => Err(format!(
            "Cargo binary {binary:?} is ambiguous; select it with --package"
        )),
    }
}

fn metadata_workspace_members(metadata: &Value) -> Result<BTreeSet<String>, String> {
    metadata["workspace_members"]
        .as_array()
        .ok_or_else(|| "cargo metadata has no workspace member list".to_owned())?
        .iter()
        .map(|member| {
            member
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "Cargo workspace member id is not text".to_owned())
        })
        .collect()
}

fn dependency_digests(
    metadata: &Value,
    packages: &BTreeMap<String, Package>,
    root: &str,
    output: &Path,
    symbols: &Path,
    target_dir: Option<&Path>,
) -> Result<Vec<PackageDigest>, String> {
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or_else(|| "cargo metadata has no resolved dependency graph".to_owned())?;
    let dependencies = nodes
        .iter()
        .map(|node| {
            let id = node["id"]
                .as_str()
                .ok_or_else(|| "Cargo dependency node has no id".to_owned())?;
            let dependencies = node["deps"]
                .as_array()
                .ok_or_else(|| "Cargo dependency node has no dependency list".to_owned())?
                .iter()
                .map(|dependency| {
                    let package = dependency["pkg"]
                        .as_str()
                        .ok_or_else(|| "Cargo dependency has no package id".to_owned())?;
                    let kinds = dependency["dep_kinds"]
                        .as_array()
                        .ok_or_else(|| "Cargo dependency has no kind list".to_owned())?;
                    Ok(kinds
                        .iter()
                        .any(|kind| kind["kind"].as_str() != Some("dev"))
                        .then(|| package.to_owned()))
                })
                .collect::<Result<Vec<_>, String>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            Ok((id.to_owned(), dependencies))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let mut pending = VecDeque::from([root.to_owned()]);
    let mut selected = BTreeSet::new();
    while let Some(id) = pending.pop_front() {
        if !selected.insert(id.clone()) {
            continue;
        }
        if let Some(next) = dependencies.get(&id) {
            pending.extend(next.iter().cloned());
        }
    }
    let metadata_target = metadata["target_directory"].as_str().map(PathBuf::from);
    let excluded_dirs = [metadata_target.as_deref(), target_dir, Some(symbols)]
        .into_iter()
        .flatten()
        .filter_map(|path| canonical_or_absolute(path).ok())
        .collect::<Vec<_>>();
    let excluded_files = [output, &appended_path(output, ".theseus-coverage.json")]
        .into_iter()
        .filter_map(|path| canonical_or_absolute(path).ok())
        .collect::<Vec<_>>();
    let mut result = selected
        .into_iter()
        .map(|id| {
            let package = packages
                .get(&id)
                .ok_or_else(|| format!("resolved Cargo package {id:?} has no metadata"))?;
            let root = package.manifest_path.parent().ok_or_else(|| {
                format!(
                    "Cargo manifest has no parent: {}",
                    package.manifest_path.display()
                )
            })?;
            let canonical_root = fs::canonicalize(root).map_err(|error| {
                format!(
                    "cannot resolve package directory {}: {error}",
                    root.display()
                )
            })?;
            if excluded_dirs
                .iter()
                .any(|excluded| canonical_root.starts_with(excluded))
            {
                return Err(format!(
                    "coverage output directories must not contain Cargo package {}",
                    package.name
                ));
            }
            Ok(PackageDigest {
                name: package.name.clone(),
                version: package.version.clone(),
                source_sha256: package_tree_sha256(
                    root,
                    package.source.as_deref(),
                    &excluded_dirs,
                    &excluded_files,
                )?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    result.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.version.cmp(&right.version))
            .then_with(|| left.source_sha256.cmp(&right.source_sha256))
    });
    Ok(result)
}

fn workspace_configuration_sha256(metadata: &Value) -> Result<String, String> {
    let root = metadata["workspace_root"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata has no workspace root".to_owned())?;
    let mut digest = Sha256::new();
    digest.update(b"theseus-cargo-workspace-v1\0");
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config",
        ".cargo/config.toml",
        "rust-toolchain",
        "rust-toolchain.toml",
    ] {
        let path = root.join(relative);
        if path.is_file() {
            digest.update(relative.as_bytes());
            digest.update(b"\0");
            hash_file_into(&path, &mut digest)?;
            digest.update(b"\0");
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn package_tree_sha256(
    root: &Path,
    source: Option<&str>,
    excluded_dirs: &[PathBuf],
    excluded_files: &[PathBuf],
) -> Result<String, String> {
    let root = fs::canonicalize(root).map_err(|error| {
        format!(
            "cannot resolve package directory {}: {error}",
            root.display()
        )
    })?;
    let mut files = Vec::new();
    collect_package_files(&root, excluded_dirs, excluded_files, &mut files)?;
    files.sort();
    let mut digest = Sha256::new();
    digest.update(b"theseus-cargo-package-v1\0");
    if let Some(source) = source {
        digest.update(source.as_bytes());
    }
    digest.update(b"\0");
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .expect("collected package file belongs to its root");
        digest.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        digest.update(b"\0");
        hash_file_into(&path, &mut digest)?;
        digest.update(b"\0");
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn collect_package_files(
    directory: &Path,
    excluded_dirs: &[PathBuf],
    excluded_files: &[PathBuf],
    result: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "cannot read package directory {}: {error}",
                directory.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read package directory entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "Cargo package input symlinks are not supported: {}",
                path.display()
            ));
        }
        if file_type.is_dir() {
            if matches!(entry.file_name().to_str(), Some(".git" | "target")) {
                continue;
            }
            let canonical = fs::canonicalize(&path)
                .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
            if excluded_dirs
                .iter()
                .any(|excluded| canonical.starts_with(excluded))
            {
                continue;
            }
            collect_package_files(&canonical, excluded_dirs, excluded_files, result)?;
        } else if file_type.is_file() {
            let canonical = fs::canonicalize(&path)
                .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
            if excluded_files.contains(&canonical) {
                continue;
            }
            result.push(canonical);
        }
    }
    Ok(())
}

fn compile_runtime(
    source: &Path,
    object: &Path,
    process: &str,
    module: &str,
    build_sha256: &str,
    target: &str,
) -> Result<(), String> {
    let mut command = Command::new("clang");
    command.args(["-O2", "-fPIC", "-fvisibility=hidden"]);
    if target.starts_with("aarch64") {
        command.arg("-mno-outline-atomics");
    }
    command.arg(format!("-DTHESEUS_COVERAGE_PROCESS=\"{process}\""));
    command.arg(format!("-DTHESEUS_COVERAGE_MODULE=\"{module}\""));
    command.arg(format!(
        "-DTHESEUS_COVERAGE_BUILD_SHA256=\"{build_sha256}\""
    ));
    command.arg("-c").arg(source).arg("-o").arg(object);
    let output = command
        .output()
        .map_err(|error| format!("cannot compile coverage runtime: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure("clang coverage runtime", &output))
    }
}

#[allow(clippy::too_many_arguments)]
fn build_cargo_binary(
    options: &Options,
    manifest: &Path,
    package: &Package,
    target: &str,
    target_dir: &Path,
    runtime_object: &Path,
    build_sha256: &str,
    workspace_root: &Path,
) -> Result<PathBuf, String> {
    let executable =
        env::current_exe().map_err(|error| format!("cannot locate theseus: {error}"))?;
    let mut command = Command::new("cargo");
    command.args(["build", "--manifest-path"]);
    command.arg(manifest);
    command.args(["--package", &package.name, "--bin", &options.binary]);
    command.args([
        "--target",
        target,
        "--message-format",
        "json-render-diagnostics",
    ]);
    if options.release {
        command.arg("--release");
    }
    cargo_feature_args(&mut command, options);
    command
        .env("RUSTC_WRAPPER", &executable)
        .env(WRAPPER, "1")
        .env(BUILD_SHA256, build_sha256)
        .env(PRIMARY_CRATE, options.binary.replace('-', "_"))
        .env(RUNTIME_OBJECT, runtime_object)
        .env(WORKSPACE_ROOT, workspace_root)
        .env("RUSTC", "rustc")
        .env("CARGO_ENCODED_RUSTFLAGS", "")
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TARGET_DIR", target_dir);
    let output = command
        .output()
        .map_err(|error| format!("cannot start cargo build: {error}"))?;
    if !output.status.success() {
        return Err(cargo_build_failure(&output));
    }
    let artifacts = String::from_utf8(output.stdout)
        .map_err(|_| "cargo build emitted non-UTF-8 JSON".to_owned())?
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| message["target"]["name"] == options.binary)
        .filter(|message| {
            message["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .filter_map(|message| message["executable"].as_str().map(PathBuf::from))
        .collect::<Vec<_>>();
    match artifacts.as_slice() {
        [artifact] => fs::canonicalize(artifact).map_err(|error| {
            format!(
                "cannot resolve Cargo executable {}: {error}",
                artifact.display()
            )
        }),
        [] => Err("cargo build did not report the selected executable".to_owned()),
        _ => Err("cargo build reported the selected executable more than once".to_owned()),
    }
}

fn cargo_feature_args(command: &mut Command, options: &Options) {
    if options.locked {
        command.arg("--locked");
    }
    if options.offline {
        command.arg("--offline");
    }
    if options.no_default_features {
        command.arg("--no-default-features");
    }
    if let Some(features) = &options.features {
        command.args(["--features", features]);
    }
}

fn wrapper_should_instrument(args: &[OsString]) -> bool {
    if rustc_arg_value(args, "--crate-name").is_none()
        || rustc_arg_value(args, "--target").is_none()
    {
        return false;
    }
    let types = rustc_crate_types(args);
    !types.contains("proc-macro") && !types.contains("dylib") && !types.contains("cdylib")
}

fn rustc_crate_types(args: &[OsString]) -> BTreeSet<String> {
    let mut values = Vec::new();
    for (index, argument) in args.iter().enumerate() {
        if argument == "--crate-type" {
            if let Some(value) = args.get(index + 1).and_then(|value| value.to_str()) {
                values.push(value);
            }
        } else if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--crate-type="))
        {
            values.push(value);
        }
    }
    if values.is_empty() {
        values.push("bin");
    }
    values
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::to_owned)
        .collect()
}

fn rustc_arg_value(args: &[OsString], name: &str) -> Option<String> {
    for (index, argument) in args.iter().enumerate() {
        if argument == name {
            return args
                .get(index + 1)
                .and_then(|value| value.to_str())
                .map(str::to_owned);
        }
        if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix(&format!("{name}=")))
        {
            return Some(value.to_owned());
        }
    }
    None
}

fn require_command(command: &str) -> Result<(), String> {
    let output = Command::new(command)
        .arg("--version")
        .output()
        .map_err(|_| format!("{command} is required"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{command} is required but --version failed"))
    }
}

fn command_text(command: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(command)
        .args(args)
        .output()
        .map_err(|error| format!("cannot start {command}: {error}"))?;
    if !output.status.success() {
        return Err(command_failure(command, &output));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("{command} emitted non-UTF-8 output"))
}

fn command_failure(name: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    format!("{name} failed with {}\n{stderr}{stdout}", output.status)
}

fn cargo_build_failure(output: &std::process::Output) -> String {
    let mut detail = String::from_utf8_lossy(&output.stderr).into_owned();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(message) if message["reason"] == "compiler-message" => {
                if let Some(rendered) = message["message"]["rendered"].as_str() {
                    detail.push_str(rendered);
                }
            }
            Err(_) => {
                detail.push_str(line);
                detail.push('\n');
            }
            _ => {}
        }
    }
    format!("cargo build failed with {}\n{detail}", output.status)
}

fn gnu_build_id(binary: &Path) -> Result<String, String> {
    let output = Command::new("readelf")
        .arg("-n")
        .arg(binary)
        .output()
        .map_err(|error| format!("cannot inspect GNU build ID: {error}"))?;
    if !output.status.success() {
        return Err(command_failure("readelf", &output));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let id = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build ID: "))
        .ok_or_else(|| "Cargo binary has no GNU build ID".to_owned())?;
    if id.is_empty()
        || id.len() > 128
        || id.len() % 2 != 0
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("Cargo binary has an invalid GNU build ID".to_owned());
    }
    Ok(id.to_owned())
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut digest = Sha256::new();
    hash_file_into(path, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn hash_file_into(path: &Path, digest: &mut Sha256) -> Result<(), String> {
    let mut file = File::open(path)
        .map_err(|error| format!("cannot read build input {}: {error}", path.display()))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read build input {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(())
}

fn absolute_output(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|error| format!("cannot read current directory: {error}"))
    }
}

fn canonical_or_absolute(path: &Path) -> Result<PathBuf, String> {
    let absolute = absolute_output(path)?;
    Ok(fs::canonicalize(&absolute).unwrap_or(absolute))
}

fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_instruments_target_libraries_but_not_host_or_dynamic_crates() {
        let target = [
            "--crate-name",
            "logic",
            "--crate-type",
            "lib",
            "--target",
            "x86_64-unknown-linux-gnu",
        ]
        .map(OsString::from);
        assert!(wrapper_should_instrument(&target));
        let host =
            ["--crate-name", "build_script_build", "--crate-type", "bin"].map(OsString::from);
        assert!(!wrapper_should_instrument(&host));
        let proc_macro = [
            "--crate-name",
            "macros",
            "--crate-type",
            "proc-macro",
            "--target",
            "x86_64-unknown-linux-gnu",
        ]
        .map(OsString::from);
        assert!(!wrapper_should_instrument(&proc_macro));
        let mixed_dynamic = [
            "--crate-name",
            "plugin",
            "--crate-type",
            "rlib",
            "--crate-type=cdylib",
            "--target",
            "x86_64-unknown-linux-gnu",
        ]
        .map(OsString::from);
        assert!(!wrapper_should_instrument(&mixed_dynamic));
    }

    #[test]
    fn parses_cargo_coverage_options_in_any_order() {
        let options = parse_options(&[
            "--bin".to_owned(),
            "api-server".to_owned(),
            "--output".to_owned(),
            "work/api".to_owned(),
            "--module".to_owned(),
            "command".to_owned(),
            "--process".to_owned(),
            "api".to_owned(),
            "--symbols".to_owned(),
            "work/symbols".to_owned(),
            "--release".to_owned(),
            "--locked".to_owned(),
        ])
        .unwrap();
        assert_eq!(options.binary, "api-server");
        assert!(options.release);
        assert!(options.locked);
    }
}
