// Copyright 2026 Adrian Mârza and contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Build one Go command with first-hit basic-block coverage across the packages
//! in its main module. The build happens from an isolated source copy, so the
//! user's module is never rewritten in place.

use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const GO_COVERAGE_USAGE: &str = "Usage:
  theseus coverage go --process NAME --module NAME --package PACKAGE --symbols DIR --output FILE
      [--goarch amd64|arm64] [--tags TAGS] [--mod readonly|vendor] [--offline]
      [--target-dir DIR]";

#[derive(Debug)]
struct Options {
    process: String,
    module: String,
    package: String,
    symbols: PathBuf,
    output: PathBuf,
    target_dir: Option<PathBuf>,
    goarch: Option<String>,
    tags: Option<String>,
    mod_mode: String,
    offline: bool,
}

#[derive(Debug, Serialize)]
pub struct GoCoverageOutput {
    pub binary: String,
    pub manifest: String,
    pub symbols: String,
    pub build_sha256: String,
    pub packages: usize,
    pub instrumented_packages: usize,
    pub blocks: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct GoPackage {
    #[serde(rename = "ImportPath")]
    import_path: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Dir")]
    dir: PathBuf,
    #[serde(rename = "Standard", default)]
    standard: bool,
    #[serde(rename = "DepOnly", default)]
    dependency_only: bool,
    #[serde(rename = "Module")]
    module: Option<GoModule>,
    #[serde(rename = "GoFiles", default)]
    go_files: Vec<String>,
    #[serde(rename = "CgoFiles", default)]
    cgo_files: Vec<String>,
    #[serde(rename = "CFiles", default)]
    c_files: Vec<String>,
    #[serde(rename = "CXXFiles", default)]
    cxx_files: Vec<String>,
    #[serde(rename = "MFiles", default)]
    objective_c_files: Vec<String>,
    #[serde(rename = "HFiles", default)]
    header_files: Vec<String>,
    #[serde(rename = "FFiles", default)]
    fortran_files: Vec<String>,
    #[serde(rename = "SFiles", default)]
    assembly_files: Vec<String>,
    #[serde(rename = "SwigFiles", default)]
    swig_files: Vec<String>,
    #[serde(rename = "SwigCXXFiles", default)]
    swig_cxx_files: Vec<String>,
    #[serde(rename = "SysoFiles", default)]
    object_files: Vec<String>,
    #[serde(rename = "EmbedFiles", default)]
    embed_files: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GoModule {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Version", default)]
    version: String,
    #[serde(rename = "Sum", default)]
    sum: String,
    #[serde(rename = "Dir")]
    dir: PathBuf,
    #[serde(rename = "GoMod")]
    go_mod: PathBuf,
    #[serde(rename = "Main", default)]
    main: bool,
    #[serde(rename = "Replace")]
    replacement: Option<Box<GoModule>>,
}

#[derive(Debug, Clone, Serialize)]
struct PackageDigest {
    import_path: String,
    source_sha256: String,
    instrumented: bool,
}

#[derive(Debug, Serialize)]
struct CoverageManifest<'a> {
    format: &'static str,
    coverage: &'static str,
    language: &'static str,
    process: &'a str,
    module: &'a str,
    build_sha256: &'a str,
    compiler: &'a str,
    target: &'a str,
    symbols: &'a str,
    maximum_blocks: usize,
    go: GoManifest<'a>,
    sources: &'a [PackageDigest],
}

#[derive(Debug, Serialize)]
struct GoManifest<'a> {
    package: &'a str,
    module: &'a str,
    packages: usize,
    instrumented_packages: usize,
    module_sha256: &'a str,
    cgo_enabled: bool,
}

pub fn go_coverage(args: &[String]) -> Result<GoCoverageOutput, String> {
    let options = parse_options(args)?;

    let go_version = go_command_text(&["version"])?;
    let goarch = options
        .goarch
        .clone()
        .map(Ok)
        .unwrap_or_else(|| go_env("GOARCH"))?;
    if !matches!(goarch.as_str(), "amd64" | "arm64") {
        return Err(format!("unsupported Go architecture {goarch:?}"));
    }

    let go_mod = PathBuf::from(go_env("GOMOD")?);
    if go_mod == Path::new("/dev/null") {
        return Err("run Go coverage from inside a module with a go.mod file".to_owned());
    }
    let original_module_root = go_mod
        .parent()
        .ok_or_else(|| "go env GOMOD returned an invalid path".to_owned())?;
    let original_module_root = fs::canonicalize(original_module_root).map_err(|error| {
        format!(
            "cannot resolve Go module {}: {error}",
            original_module_root.display()
        )
    })?;
    let invocation_dir = fs::canonicalize(
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?,
    )
    .map_err(|error| format!("cannot resolve current directory: {error}"))?;
    if !invocation_dir.starts_with(&original_module_root) {
        return Err("run Go coverage from inside the selected module".to_owned());
    }

    let output = absolute_path(&options.output)?;
    let symbols = absolute_path(&options.symbols)?;
    let target_base = match options.target_dir.as_deref() {
        Some(path) => absolute_path(path)?,
        None => original_module_root.join("target/theseus-go-coverage"),
    };
    fs::create_dir_all(&target_base)
        .map_err(|error| format!("cannot create {}: {error}", target_base.display()))?;
    let packages = go_list(
        &options,
        &goarch,
        &original_module_root,
        &invocation_dir,
        &target_base.join("list-cache"),
    )?;
    let selected = select_command(&packages)?;
    let selected_module = selected
        .module
        .as_ref()
        .filter(|module| module.main)
        .ok_or_else(|| "the selected Go command must belong to the main module".to_owned())?;
    let module_root = fs::canonicalize(&selected_module.dir).map_err(|error| {
        format!(
            "cannot resolve Go module {}: {error}",
            selected_module.dir.display()
        )
    })?;
    if module_root != original_module_root {
        return Err("the selected Go command must belong to the current module".to_owned());
    }
    reject_external_local_replacements(&packages, &module_root)?;

    let manifest_output = appended_path(&output, ".theseus-coverage.json");
    let module_files = local_build_files(&packages, &module_root)?;
    let module_sha256 = tree_sha256(&module_root, &module_files, b"theseus-go-module-v1\0")?;
    let package_digests = package_digests(&packages, &module_root)?;
    let instrumented_packages = package_digests
        .iter()
        .filter(|package| package.instrumented)
        .count();
    if instrumented_packages == 0 {
        return Err("the selected Go module has no instrumentable packages".to_owned());
    }

    let frontend_sha256 = file_sha256(
        &env::current_exe().map_err(|error| format!("cannot locate theseus: {error}"))?,
    )?;
    let ledger = serde_json::json!({
        "format": "theseus-go-coverage-input-v1",
        "process": options.process,
        "module": options.module,
        "package": selected.import_path,
        "goarch": goarch,
        "tags": options.tags,
        "mod": options.mod_mode,
        "go": go_version,
        "frontend_sha256": frontend_sha256,
        "module_sha256": module_sha256,
        "packages": package_digests,
    });
    let build_sha256 = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&ledger)
                .map_err(|error| format!("cannot encode Go build inputs: {error}"))?
        )
    );
    let target_dir = target_base.join(&build_sha256[..16]);
    fs::create_dir_all(&target_dir)
        .map_err(|error| format!("cannot create {}: {error}", target_dir.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("cannot create isolated Go workspace: {error}"))?
        .as_nanos();
    let source_copy = target_dir.join(format!(".source-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&source_copy)
        .map_err(|error| format!("cannot create isolated Go source directory: {error}"))?;

    let build = (|| {
        copy_tree(&module_root, &source_copy, &module_files)?;
        let blocks = instrument_module_packages(
            &packages,
            &module_root,
            &source_copy,
            &options.process,
            &options.module,
            &build_sha256,
        )?;
        if blocks == 0 {
            return Err("the selected Go command has no instrumented blocks".to_owned());
        }
        build_go_command(
            &options,
            selected,
            &source_copy,
            &output,
            &target_dir,
            &goarch,
            &build_sha256,
        )?;
        Ok(blocks)
    })();
    let _ = fs::remove_dir_all(&source_copy);
    let blocks = build?;

    let binary = fs::read(&output)
        .map_err(|error| format!("cannot read Go binary {}: {error}", output.display()))?;
    if !binary
        .windows(build_sha256.len())
        .any(|window| window == build_sha256.as_bytes())
    {
        return Err("instrumented Go binary does not contain its build identity".to_owned());
    }
    if !binary.starts_with(b"\x7fELF") {
        return Err("instrumented Go output is not an ELF binary".to_owned());
    }
    fs::create_dir_all(&symbols)
        .map_err(|error| format!("cannot create {}: {error}", symbols.display()))?;
    let symbol_name = format!("{}-{build_sha256}.debug", options.module);
    let symbol_path = symbols.join(&symbol_name);
    fs::copy(&output, &symbol_path)
        .map_err(|error| format!("cannot preserve {}: {error}", symbol_path.display()))?;

    let target = format!("linux/{goarch}");
    let manifest = CoverageManifest {
        format: "theseus-go-coverage-build-v1",
        coverage: "blocks",
        language: "go",
        process: &options.process,
        module: &options.module,
        build_sha256: &build_sha256,
        compiler: go_version.trim(),
        target: &target,
        symbols: &symbol_name,
        maximum_blocks: blocks,
        go: GoManifest {
            package: &selected.import_path,
            module: &selected_module.path,
            packages: package_digests.len(),
            instrumented_packages,
            module_sha256: &module_sha256,
            cgo_enabled: false,
        },
        sources: &package_digests,
    };
    fs::write(
        &manifest_output,
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("cannot encode Go coverage manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot write {}: {error}", manifest_output.display()))?;

    Ok(GoCoverageOutput {
        binary: output.display().to_string(),
        manifest: manifest_output.display().to_string(),
        symbols: symbol_path.display().to_string(),
        build_sha256,
        packages: package_digests.len(),
        instrumented_packages,
        blocks,
    })
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut process = None;
    let mut module = None;
    let mut package = None;
    let mut symbols = None;
    let mut output = None;
    let mut target_dir = None;
    let mut goarch = None;
    let mut tags = None;
    let mut mod_mode = "readonly".to_owned();
    let mut offline = false;
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        let value = |index: &mut usize| -> Result<String, String> {
            *index += 1;
            args.get(*index)
                .cloned()
                .ok_or_else(|| GO_COVERAGE_USAGE.to_owned())
        };
        match flag.as_str() {
            "--process" => process = Some(value(&mut index)?),
            "--module" => module = Some(value(&mut index)?),
            "--package" => package = Some(value(&mut index)?),
            "--symbols" => symbols = Some(PathBuf::from(value(&mut index)?)),
            "--output" => output = Some(PathBuf::from(value(&mut index)?)),
            "--target-dir" => target_dir = Some(PathBuf::from(value(&mut index)?)),
            "--goarch" => goarch = Some(value(&mut index)?),
            "--tags" => tags = Some(value(&mut index)?),
            "--mod" => mod_mode = value(&mut index)?,
            "--offline" => offline = true,
            _ => return Err(GO_COVERAGE_USAGE.to_owned()),
        }
        index += 1;
    }
    let process = process.ok_or_else(|| GO_COVERAGE_USAGE.to_owned())?;
    let module = module.ok_or_else(|| GO_COVERAGE_USAGE.to_owned())?;
    validate_identity("process", &process)?;
    validate_identity("module", &module)?;
    if !matches!(mod_mode.as_str(), "readonly" | "vendor") {
        return Err("--mod must be readonly or vendor".to_owned());
    }
    if tags.as_deref().is_some_and(str::is_empty) {
        return Err("--tags must not be empty".to_owned());
    }
    Ok(Options {
        process,
        module,
        package: package.ok_or_else(|| GO_COVERAGE_USAGE.to_owned())?,
        symbols: symbols.ok_or_else(|| GO_COVERAGE_USAGE.to_owned())?,
        output: output.ok_or_else(|| GO_COVERAGE_USAGE.to_owned())?,
        target_dir,
        goarch,
        tags,
        mod_mode,
        offline,
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

fn go_list(
    options: &Options,
    goarch: &str,
    module_root: &Path,
    invocation_dir: &Path,
    cache: &Path,
) -> Result<Vec<GoPackage>, String> {
    let alternate = AlternateModuleFiles::create(module_root)?;
    let mut command = go_command(goarch, options.offline);
    command.args(["list", "-deps", "-json"]);
    command.arg(format!("-mod={}", options.mod_mode));
    command.arg("-modfile").arg(&alternate.mod_file);
    if let Some(tags) = &options.tags {
        command.args(["-tags", tags]);
    }
    command
        .arg(&options.package)
        .env("GOCACHE", cache)
        .current_dir(invocation_dir);
    let output = command
        .output()
        .map_err(|error| format!("cannot start go list: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "go list failed with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter::<GoPackage>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("go list returned invalid JSON: {error}"))
}

struct AlternateModuleFiles {
    mod_file: PathBuf,
    sum_file: PathBuf,
}

impl AlternateModuleFiles {
    fn create(module_root: &Path) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("cannot prepare isolated Go module metadata: {error}"))?
            .as_nanos();
        let stem = format!(".theseus-go-{}-{nonce}", std::process::id());
        let mod_file = module_root.join(format!("{stem}.mod"));
        let sum_file = module_root.join(format!("{stem}.sum"));
        copy_new_file(&module_root.join("go.mod"), &mod_file, "go.mod")?;
        let source_sum = module_root.join("go.sum");
        if source_sum.is_file() {
            if let Err(error) = copy_new_file(&source_sum, &sum_file, "go.sum") {
                let _ = fs::remove_file(&mod_file);
                return Err(error);
            }
        }
        Ok(Self { mod_file, sum_file })
    }
}

fn copy_new_file(source: &Path, destination_path: &Path, name: &str) -> Result<(), String> {
    let mut source = File::open(source).map_err(|error| format!("cannot read {name}: {error}"))?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination_path)
        .map_err(|error| format!("cannot isolate {name}: {error}"))?;
    if let Err(error) = io::copy(&mut source, &mut destination) {
        drop(destination);
        let _ = fs::remove_file(destination_path);
        return Err(format!("cannot isolate {name}: {error}"));
    }
    Ok(())
}

impl Drop for AlternateModuleFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.mod_file);
        let _ = fs::remove_file(&self.sum_file);
    }
}

fn select_command(packages: &[GoPackage]) -> Result<&GoPackage, String> {
    let selected = packages
        .iter()
        .filter(|package| !package.dependency_only)
        .collect::<Vec<_>>();
    match selected.as_slice() {
        [package] if package.name == "main" => Ok(package),
        [package] => Err(format!(
            "selected Go package {:?} is not a command",
            package.import_path
        )),
        [] => Err("go list did not return the selected package".to_owned()),
        _ => Err("go list returned more than one selected package".to_owned()),
    }
}

fn reject_external_local_replacements(
    packages: &[GoPackage],
    module_root: &Path,
) -> Result<(), String> {
    for package in packages.iter().filter(|package| !package.standard) {
        let Some(replacement) = package
            .module
            .as_ref()
            .and_then(|module| module.replacement.as_deref())
        else {
            continue;
        };
        if replacement.version.is_empty() {
            let replacement_dir = fs::canonicalize(&replacement.dir).map_err(|error| {
                format!(
                    "cannot resolve replacement module {}: {error}",
                    replacement.dir.display()
                )
            })?;
            if !replacement_dir.starts_with(module_root) {
                return Err(format!(
                    "local replacement module {} is outside the selected module",
                    replacement_dir.display()
                ));
            }
        }
    }
    Ok(())
}

fn package_digests(
    packages: &[GoPackage],
    module_root: &Path,
) -> Result<Vec<PackageDigest>, String> {
    let mut result = packages
        .iter()
        .filter(|package| !package.standard)
        .map(|package| {
            let directory = fs::canonicalize(&package.dir).map_err(|error| {
                format!(
                    "cannot resolve Go package {}: {error}",
                    package.dir.display()
                )
            })?;
            let mut names = package_input_names(package);
            names.sort();
            names.dedup();
            let mut digest = Sha256::new();
            digest.update(b"theseus-go-package-v1\0");
            digest.update(package.import_path.as_bytes());
            digest.update(b"\0");
            if let Some(module) = &package.module {
                digest.update(module.path.as_bytes());
                digest.update(b"\0");
                digest.update(module.version.as_bytes());
                digest.update(b"\0");
                digest.update(module.sum.as_bytes());
                digest.update(b"\0");
            }
            for name in names {
                let path = directory.join(&name);
                if fs::symlink_metadata(&path)
                    .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
                    .file_type()
                    .is_symlink()
                {
                    return Err(format!(
                        "Go package input symlinks are not supported: {}",
                        path.display()
                    ));
                }
                digest.update(name.replace('\\', "/").as_bytes());
                digest.update(b"\0");
                hash_file_into(&path, &mut digest)?;
                digest.update(b"\0");
            }
            Ok(PackageDigest {
                import_path: package.import_path.clone(),
                source_sha256: format!("{:x}", digest.finalize()),
                instrumented: directory.starts_with(module_root)
                    && package.module.as_ref().is_some_and(|module| module.main),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    result.sort_by(|left, right| left.import_path.cmp(&right.import_path));
    Ok(result)
}

fn package_input_names(package: &GoPackage) -> Vec<String> {
    [
        &package.go_files,
        &package.cgo_files,
        &package.c_files,
        &package.cxx_files,
        &package.objective_c_files,
        &package.header_files,
        &package.fortran_files,
        &package.assembly_files,
        &package.swig_files,
        &package.swig_cxx_files,
        &package.object_files,
        &package.embed_files,
    ]
    .into_iter()
    .flat_map(|names| names.iter().cloned())
    .collect()
}

fn instrument_module_packages(
    packages: &[GoPackage],
    module_root: &Path,
    source_copy: &Path,
    process: &str,
    module: &str,
    build_sha256: &str,
) -> Result<usize, String> {
    let mut blocks = 0;
    let mut selected = packages
        .iter()
        .filter(|package| !package.standard)
        .filter_map(|package| {
            if !package.module.as_ref().is_some_and(|module| module.main) {
                return None;
            }
            let directory = fs::canonicalize(&package.dir).ok()?;
            directory
                .starts_with(module_root)
                .then_some((package, directory))
        })
        .collect::<Vec<_>>();
    selected.sort_by(|(left, _), (right, _)| left.import_path.cmp(&right.import_path));
    for (package, directory) in selected {
        let package_relative = directory
            .strip_prefix(module_root)
            .map_err(|_| format!("Go package {} escaped its module", package.import_path))?;
        let mut files = package.go_files.clone();
        files.sort();
        for file in files {
            let relative = package_relative.join(&file);
            let path = source_copy.join(&relative);
            let file_blocks =
                instrument_go_file(source_copy, &relative, &path, process, module, build_sha256)?;
            blocks += file_blocks;
        }
    }
    Ok(blocks)
}

fn instrument_go_file(
    source_copy: &Path,
    relative: &Path,
    path: &Path,
    process: &str,
    module: &str,
    build_sha256: &str,
) -> Result<usize, String> {
    let suffix = format!(
        "{:x}",
        Sha256::digest(relative.to_string_lossy().as_bytes())
    );
    let suffix = &suffix[..12];
    let variable = format!("_TheseusCover{suffix}");
    let temporary = path.with_file_name(format!(".theseus-covered-{suffix}.go"));
    let mut command = base_go_command();
    command
        .args(["tool", "cover", "-mode=set"])
        .arg(format!("-var={variable}"))
        .arg("-o")
        .arg(&temporary)
        .arg(relative)
        .current_dir(source_copy);
    let output = command
        .output()
        .map_err(|error| format!("cannot start go tool cover: {error}"))?;
    if !output.status.success() {
        return Err(command_failure("go tool cover", &output));
    }
    let mut covered = fs::read_to_string(&temporary)
        .map_err(|error| format!("cannot read {}: {error}", temporary.display()))?;
    let counter = Regex::new(&format!(
        r"{}\.Count\[(\d+)\] = 1",
        regex::escape(&variable)
    ))
    .expect("coverage counter pattern is valid");
    let count = counter.captures_iter(&covered).count();
    if count == 0 {
        fs::rename(&temporary, path)
            .map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
        return Ok(0);
    }
    let helper = format!("_theseusCoverageHit{suffix}");
    covered = counter
        .replace_all(&covered, |captures: &Captures<'_>| {
            format!("{helper}({})", &captures[1])
        })
        .into_owned();
    let package = Regex::new(r"(?m)^package[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*$")
        .expect("Go package pattern is valid");
    let package_end = package
        .find(&covered)
        .map(|matched| matched.end())
        .ok_or_else(|| format!("covered Go file {} has no package", relative.display()))?;
    let imports = format!(
        "\n\nimport (\n\t_theseusFmt{suffix} \"fmt\"\n\t_theseusOs{suffix} \"os\"\n\t_theseusRuntime{suffix} \"runtime\"\n\t_theseusAtomic{suffix} \"sync/atomic\"\n)"
    );
    covered.insert_str(package_end, &imports);
    covered.push_str(&format!(
        "\n\nvar _theseusCoverageSeen{suffix} [{count}]uint32\n\n//go:noinline\nfunc {helper}(block int) {{\n\tif !_theseusAtomic{suffix}.CompareAndSwapUint32(&_theseusCoverageSeen{suffix}[block], 0, 1) {{\n\t\treturn\n\t}}\n\tvar pcs [1]uintptr\n\tif _theseusRuntime{suffix}.Callers(2, pcs[:]) == 1 && pcs[0] != 0 {{\n\t\t_theseusFmt{suffix}.Fprintf(_theseusOs{suffix}.Stderr, \"THES:COV:v1:{process}:{module}:{build_sha256}:0x%x\\n\", pcs[0]-1)\n\t}}\n}}\n"
    ));
    fs::write(path, covered)
        .map_err(|error| format!("cannot write instrumented {}: {error}", path.display()))?;
    fs::remove_file(&temporary)
        .map_err(|error| format!("cannot remove {}: {error}", temporary.display()))?;
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
fn build_go_command(
    options: &Options,
    selected: &GoPackage,
    source_copy: &Path,
    output: &Path,
    target_dir: &Path,
    goarch: &str,
    build_sha256: &str,
) -> Result<(), String> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mut command = go_command(goarch, options.offline);
    command
        .arg("build")
        .arg(format!("-mod={}", options.mod_mode))
        .args(["-trimpath", "-buildvcs=false", "-buildmode=exe"])
        .arg(format!("-ldflags=-buildid={build_sha256}"));
    if let Some(tags) = &options.tags {
        command.args(["-tags", tags]);
    }
    command
        .arg("-o")
        .arg(output)
        .arg(&selected.import_path)
        .env("GOWORK", "off")
        .env("GOCACHE", target_dir.join("cache"))
        .current_dir(source_copy);
    let result = command
        .output()
        .map_err(|error| format!("cannot start go build: {error}"))?;
    if result.status.success() {
        Ok(())
    } else {
        Err(command_failure("go build", &result))
    }
}

fn go_command(goarch: &str, offline: bool) -> Command {
    let mut command = base_go_command();
    command
        .env("GOOS", "linux")
        .env("GOARCH", goarch)
        .env("CGO_ENABLED", "0")
        .env("GOWORK", "off")
        .env_remove("GOARM64")
        .env_remove("GOAMD64");
    if goarch == "amd64" {
        command.env("GOAMD64", "v1");
    } else {
        command.env_remove("GOAMD64");
    }
    if offline {
        command.env("GOPROXY", "off").env("GOSUMDB", "off");
    }
    command
}

fn base_go_command() -> Command {
    let mut command = Command::new("go");
    command
        .env("GOENV", "off")
        .env("GOTELEMETRY", "off")
        .env("GOTOOLCHAIN", "local")
        .env_remove("GOEXPERIMENT")
        .env_remove("GOFLAGS");
    command
}

fn go_env(name: &str) -> Result<String, String> {
    let output = base_go_command()
        .args(["env", name])
        .env("GOWORK", "off")
        .output()
        .map_err(|error| format!("cannot start go: {error}"))?;
    if !output.status.success() {
        return Err(command_failure("go", &output));
    }
    let output =
        String::from_utf8(output.stdout).map_err(|_| "go emitted non-UTF-8 output".to_owned())?;
    let value = output.trim();
    if value.is_empty() {
        Err(format!("go env {name} returned an empty value"))
    } else {
        Ok(value.to_owned())
    }
}

fn local_build_files(packages: &[GoPackage], module_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut result = BTreeSet::new();
    add_local_input(&mut result, module_root, &module_root.join("go.mod"), true)?;
    add_local_input(&mut result, module_root, &module_root.join("go.sum"), false)?;
    add_local_input(
        &mut result,
        module_root,
        &module_root.join("vendor/modules.txt"),
        false,
    )?;
    for package in packages.iter().filter(|package| !package.standard) {
        let directory = fs::canonicalize(&package.dir).map_err(|error| {
            format!(
                "cannot resolve Go package {}: {error}",
                package.dir.display()
            )
        })?;
        if !directory.starts_with(module_root) {
            continue;
        }
        for name in package_input_names(package) {
            add_local_input(&mut result, module_root, &directory.join(name), true)?;
        }
        if let Some(module) = package.module.as_ref().filter(|module| !module.main) {
            add_local_input(&mut result, module_root, &module.go_mod, false)?;
            if let Some(parent) = module.go_mod.parent() {
                add_local_input(&mut result, module_root, &parent.join("go.sum"), false)?;
            }
        }
    }
    Ok(result.into_iter().collect())
}

fn add_local_input(
    result: &mut BTreeSet<PathBuf>,
    module_root: &Path,
    path: &Path,
    required: bool,
) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "Go build input symlinks are not supported: {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!("Go build input is not a file: {}", path.display()));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    if canonical.starts_with(module_root) {
        result.insert(canonical);
    }
    Ok(())
}

fn tree_sha256(root: &Path, files: &[PathBuf], domain: &[u8]) -> Result<String, String> {
    let mut digest = Sha256::new();
    digest.update(domain);
    for path in files {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| format!("module input {} escaped its root", path.display()))?;
        digest.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        digest.update(b"\0");
        hash_file_into(path, &mut digest)?;
        digest.update(b"\0");
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn copy_tree(root: &Path, destination: &Path, files: &[PathBuf]) -> Result<(), String> {
    for source in files {
        let relative = source
            .strip_prefix(root)
            .map_err(|_| format!("module input {} escaped its root", source.display()))?;
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        fs::copy(source, &target)
            .map_err(|error| format!("cannot copy {}: {error}", source.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&target)
                .map_err(|error| format!("cannot inspect {}: {error}", target.display()))?
                .permissions();
            permissions.set_mode(permissions.mode() | 0o200);
            fs::set_permissions(&target, permissions)
                .map_err(|error| format!("cannot make {} writable: {error}", target.display()))?;
        }
    }
    Ok(())
}

fn go_command_text(args: &[&str]) -> Result<String, String> {
    let output = base_go_command()
        .args(args)
        .output()
        .map_err(|error| format!("cannot start go: {error}"))?;
    if !output.status.success() {
        return Err(command_failure("go", &output));
    }
    String::from_utf8(output.stdout).map_err(|_| "go emitted non-UTF-8 output".to_owned())
}

fn command_failure(name: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    format!("{name} failed with {}\n{stderr}{stdout}", output.status)
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

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|error| format!("cannot read current directory: {error}"))
    }
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
    fn parses_go_coverage_options() {
        let options = parse_options(&[
            "--package".to_owned(),
            "./cmd/server".to_owned(),
            "--output".to_owned(),
            "work/server".to_owned(),
            "--module".to_owned(),
            "command".to_owned(),
            "--process".to_owned(),
            "api".to_owned(),
            "--symbols".to_owned(),
            "work/symbols".to_owned(),
            "--goarch".to_owned(),
            "arm64".to_owned(),
            "--offline".to_owned(),
        ])
        .unwrap();
        assert_eq!(options.package, "./cmd/server");
        assert_eq!(options.goarch.as_deref(), Some("arm64"));
        assert!(options.offline);
    }

    #[test]
    fn rejects_mutating_module_mode() {
        let error = parse_options(&[
            "--process".to_owned(),
            "api".to_owned(),
            "--module".to_owned(),
            "command".to_owned(),
            "--package".to_owned(),
            ".".to_owned(),
            "--symbols".to_owned(),
            "symbols".to_owned(),
            "--output".to_owned(),
            "server".to_owned(),
            "--mod".to_owned(),
            "mod".to_owned(),
        ])
        .unwrap_err();
        assert!(error.contains("readonly or vendor"));
    }
}
