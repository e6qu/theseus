// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The Java coverage frontend: build the Theseus coverage agent for one
//! application JAR and write the locked coverage catalog for it.
//!
//! The agent records class-load coverage: every application class the JVM
//! defines is one coverage point, reported through the same first-hit
//! `THES:COV:v1:` serial-line protocol as the C, LLVM, and Go frontends. A
//! class is covered when the run actually loaded it - JVM classes load
//! lazily, so first load is first touch.
//!
//! Unlike the compiled frontends, the agent is generic and takes its
//! identity at attach time (`-javaagent:...=process,module,build_sha256`),
//! so one agent JAR serves every build. The frontend compiles that agent
//! with the JDK's own `javac` and `jar` tools, lists the application JAR's
//! classes with `jar tf`, derives each class's deterministic coverage-point
//! offset (the first eight SHA-256 bytes of its internal name), and locks
//! the whole set into a build digest and a symbol map.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use sha2::{Digest, Sha256};

pub const JAVA_COVERAGE_USAGE: &str = "Usage:
  theseus coverage java --process NAME --module NAME --jar FILE --symbols DIR --output FILE
      [--agent-jar FILE]";

/// The agent source packaged with this frontend. Compiled with the user's
/// JDK so the shipped agent always matches the shipped protocol.
const AGENT_SOURCE: &str = include_str!("../../instrumentation/java/TheseusCoverageAgent.java");

#[derive(Debug)]
struct Options {
    process: String,
    module: String,
    jar: PathBuf,
    symbols: PathBuf,
    output: PathBuf,
    agent_jar: PathBuf,
}

#[derive(Debug)]
pub struct JavaCoverageOutput {
    pub classes: usize,
    pub manifest: PathBuf,
    pub symbols: PathBuf,
    pub agent_jar: PathBuf,
    pub build_sha256: String,
}

#[derive(Serialize)]
struct ClassPoint {
    class: String,
    offset: String,
    source: String,
}

#[derive(Serialize)]
struct SymbolMap<'a> {
    format: &'static str,
    build_sha256: &'a str,
    classes: &'a [ClassPoint],
}

/// The deterministic coverage-point offset for one class, byte-for-byte the
/// identity the agent derives at runtime.
fn offset_for_class(internal_name: &str) -> String {
    let digest = Sha256::digest(internal_name.as_bytes());
    let mut offset = String::from("0x");
    for byte in &digest[..8] {
        offset.push_str(&format!("{byte:02x}"));
    }
    offset
}

fn valid_identity(field: &str) -> bool {
    !field.is_empty()
        && field.len() <= 64
        && field
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    if args.is_empty() {
        return Err(JAVA_COVERAGE_USAGE.to_owned());
    }
    let mut process: Option<String> = None;
    let mut module: Option<String> = None;
    let mut jar: Option<PathBuf> = None;
    let mut symbols: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut agent_jar: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        let take = |index: usize| -> Result<String, String> {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| JAVA_COVERAGE_USAGE.to_owned())
        };
        match args[index].as_str() {
            "--process" => process = Some(take(index)?),
            "--module" => module = Some(take(index)?),
            "--jar" => jar = Some(PathBuf::from(take(index)?)),
            "--symbols" => symbols = Some(PathBuf::from(take(index)?)),
            "--output" => output = Some(PathBuf::from(take(index)?)),
            "--agent-jar" => agent_jar = Some(PathBuf::from(take(index)?)),
            "--help" | "-h" => return Err(JAVA_COVERAGE_USAGE.to_owned()),
            _ => return Err(JAVA_COVERAGE_USAGE.to_owned()),
        }
        index += 2;
    }
    let process = process.ok_or_else(|| JAVA_COVERAGE_USAGE.to_owned())?;
    let module = module.ok_or_else(|| JAVA_COVERAGE_USAGE.to_owned())?;
    let jar = jar.ok_or_else(|| JAVA_COVERAGE_USAGE.to_owned())?;
    let symbols = symbols.ok_or_else(|| JAVA_COVERAGE_USAGE.to_owned())?;
    let output = output.ok_or_else(|| JAVA_COVERAGE_USAGE.to_owned())?;
    if !valid_identity(&process) || !valid_identity(&module) {
        return Err(format!(
            "coverage identity names must be 1-64 characters of [A-Za-z0-9._-]: {process:?}, {module:?}"
        ));
    }
    let agent_jar = agent_jar.unwrap_or_else(|| {
        output
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("theseus-coverage-agent.jar")
    });
    // Every later tool call runs inside the isolated work directory, so the
    // user-facing paths must be resolved before that happens.
    let absolutize = |path: PathBuf| -> PathBuf {
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    };
    Ok(Options {
        process,
        module,
        jar: absolutize(jar),
        symbols: absolutize(symbols),
        output: absolutize(output),
        agent_jar: absolutize(agent_jar),
    })
}

fn tool(program: &str, arguments: &[&str], working: &Path) -> Result<(), String> {
    let status = Command::new(program)
        .args(arguments)
        .current_dir(working)
        .status()
        .map_err(|error| {
            format!(
                "cannot run {program}: {error}; the Java frontend needs a JDK with javac and jar on PATH"
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {status}"))
    }
}

/// List the application JAR's classes as internal names, in deterministic
/// order, using the JDK's own archive tool.
fn jar_class_names(jar: &Path) -> Result<Vec<String>, String> {
    let listing = Command::new("jar")
        .arg("--list")
        .arg("--file")
        .arg(jar)
        .output()
        .map_err(|error| {
            format!("cannot run jar: {error}; the Java frontend needs a JDK on PATH")
        })?;
    if !listing.status.success() {
        return Err(format!(
            "jar --list failed for {}: {}",
            jar.display(),
            String::from_utf8_lossy(&listing.stderr).trim()
        ));
    }
    let mut names = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|entry| {
            let entry = entry.trim();
            let internal = entry.strip_suffix(".class")?;
            // Skip the module descriptor wherever it hides.
            if internal.ends_with("module-info") || internal.is_empty() {
                return None;
            }
            Some(internal.to_owned())
        })
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Build the coverage agent JAR in an isolated directory and copy it to the
/// requested output.
fn build_agent_jar(agent_jar: &Path, work: &Path) -> Result<String, String> {
    let source = work.join("TheseusCoverageAgent.java");
    fs::write(&source, AGENT_SOURCE)
        .map_err(|error| format!("cannot write the agent source: {error}"))?;
    tool("javac", &["TheseusCoverageAgent.java"], work)?;
    let manifest = work.join("manifest.txt");
    fs::write(&manifest, "Premain-Class: TheseusCoverageAgent\n")
        .map_err(|error| format!("cannot write the agent manifest: {error}"))?;
    tool(
        "jar",
        &[
            "--create",
            "--file",
            agent_jar
                .to_str()
                .ok_or_else(|| "agent jar path is not UTF-8".to_owned())?,
            "--manifest",
            "manifest.txt",
            "-C",
            ".",
            "TheseusCoverageAgent.class",
        ],
        work,
    )?;
    let bytes =
        fs::read(agent_jar).map_err(|error| format!("cannot read the built agent jar: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// Build the Theseus coverage agent for one application JAR and write its
/// locked coverage catalog: the manifest, the class-to-offset symbol map,
/// and the generic agent JAR the service command attaches.
pub fn java_coverage(args: &[String]) -> Result<JavaCoverageOutput, String> {
    let options = parse_options(args)?;
    let jar_bytes = fs::read(&options.jar).map_err(|error| {
        format!(
            "cannot read application jar {}: {error}",
            options.jar.display()
        )
    })?;
    let jar_sha256 = format!("{:x}", Sha256::digest(&jar_bytes));

    let nonce = std::process::id();
    let work = std::env::temp_dir().join(format!("theseus-java-coverage-{nonce}"));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)
        .map_err(|error| format!("cannot create the work directory: {error}"))?;
    let built = (|| {
        let agent_source_sha256 = format!("{:x}", Sha256::digest(AGENT_SOURCE.as_bytes()));
        let agent_jar_sha256 = build_agent_jar(&options.agent_jar, &work)?;
        let class_names = jar_class_names(&options.jar)?;
        if class_names.is_empty() {
            return Err(format!(
                "application jar {} retains no classes",
                options.jar.display()
            ));
        }
        let classes = class_names
            .iter()
            .map(|name| ClassPoint {
                class: name.clone(),
                offset: offset_for_class(name),
                source: format!("{name}.java"),
            })
            .collect::<Vec<_>>();

        // The build digest covers the identity, the exact agent source and
        // binary, the application JAR, and every coverage point, mirroring
        // the Go and Cargo ledgers.
        let ledger = serde_json::json!({
            "format": "theseus-java-coverage-input-v1",
            "process": options.process,
            "module": options.module,
            "agent_source_sha256": agent_source_sha256,
            "agent_jar_sha256": agent_jar_sha256,
            "jar": {
                "name": options.jar.file_name().and_then(|name| name.to_str()),
                "sha256": jar_sha256,
            },
            "classes": classes,
        });
        let build_sha256 = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&ledger).expect("ledger serializes"))
        );

        let symbol_map = SymbolMap {
            format: "theseus-java-coverage-symbols-v1",
            build_sha256: &build_sha256,
            classes: &classes,
        };
        fs::create_dir_all(&options.symbols)
            .map_err(|error| format!("cannot create the symbols directory: {error}"))?;
        let symbol_name = format!("{}-{build_sha256}.debug", options.module);
        let symbol_path = options.symbols.join(&symbol_name);
        fs::write(
            &symbol_path,
            serde_json::to_vec_pretty(&symbol_map).expect("symbol map serializes"),
        )
        .map_err(|error| {
            format!(
                "cannot write coverage symbols {}: {error}",
                symbol_path.display()
            )
        })?;

        let manifest = serde_json::json!({
            "format": "theseus-java-coverage-build-v1",
            "coverage": "classes",
            "language": "java",
            "process": options.process,
            "module": options.module,
            "build_sha256": build_sha256,
            "symbols": symbol_name,
            "agent_source_sha256": agent_source_sha256,
            "agent_jar_sha256": agent_jar_sha256,
            "jar": {
                "name": options.jar.file_name().and_then(|name| name.to_str()),
                "sha256": jar_sha256,
            },
            "classes": classes,
        });
        fs::write(
            &options.output,
            serde_json::to_vec_pretty(&manifest).expect("manifest serializes"),
        )
        .map_err(|error| {
            format!(
                "cannot write coverage manifest {}: {error}",
                options.output.display()
            )
        })?;
        Ok(JavaCoverageOutput {
            classes: classes.len(),
            manifest: options.output.clone(),
            symbols: symbol_path,
            agent_jar: options.agent_jar.clone(),
            build_sha256,
        })
    })();
    let _ = fs::remove_dir_all(&work);
    built
}
