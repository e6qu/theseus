// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pivot: the PID-1 init injected into container-image VMs.
//!
//! It mounts the essentials and reads /etc/theseus-init.json (written by the
//! image flattener). A plain image entrypoint is exec'd unchanged. With a
//! service contract, the pivot starts it as a child, waits for HTTP readiness,
//! evaluates assertions, and emits the result on the serial control channel.
//! The image needs no Theseus code of its own — the pivot is the
//! instrumentation.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::FromRawFd;
use std::thread;
use std::time::Duration;

use theseus_sdk::linux::TtyChannel;
use theseus_sdk::MARKER_BOOT;

#[derive(serde::Deserialize)]
struct InitSpec {
    argv: Vec<String>,
    env: Vec<String>,
    workdir: String,
    #[serde(default)]
    container_service: Option<ContainerService>,
    #[serde(default)]
    network: Option<ContainerNetwork>,
}

#[derive(serde::Deserialize)]
struct ContainerService {
    #[serde(default)]
    campaign: bool,
    #[serde(default)]
    ready: Option<HttpReady>,
    #[serde(default)]
    assertions: Vec<HttpAssertion>,
    #[serde(default)]
    operations: Vec<HttpOperation>,
    #[serde(default)]
    grpc_ready: Option<GrpcHealth>,
    #[serde(default)]
    grpc_assertions: Vec<GrpcAssertion>,
    #[serde(default)]
    grpc_operations: Vec<GrpcOperation>,
    #[serde(default)]
    shell_operations: Vec<ShellOperation>,
    #[serde(default)]
    network: ContainerNetwork,
}

#[derive(Default, serde::Deserialize)]
struct ContainerNetwork {
    #[serde(default)]
    interfaces: Vec<NetworkInterface>,
    #[serde(default)]
    hosts: BTreeMap<String, String>,
}

impl ContainerNetwork {
    fn is_empty(&self) -> bool {
        self.interfaces.is_empty() && self.hosts.is_empty()
    }
}

#[derive(serde::Deserialize)]
struct NetworkInterface {
    name: String,
    address: String,
    prefix_len: u8,
}

#[derive(serde::Deserialize)]
struct HttpReady {
    url: String,
    attempts: u32,
    interval_millis: u64,
}

#[derive(serde::Deserialize)]
struct HttpAssertion {
    name: String,
    url: String,
    expect_status: u16,
    #[serde(default)]
    body_contains: Option<String>,
}

#[derive(serde::Deserialize)]
struct HttpOperation {
    name: String,
    method: HttpMethod,
    url: String,
    #[serde(default)]
    body: Option<String>,
    expect_status: u16,
    #[serde(default)]
    body_contains: Option<String>,
}

/// A host sends this JSON after the boot marker. It is generated from a
/// locked Compose operation; applications never need to implement it.
#[derive(serde::Deserialize)]
struct CampaignHttpOperation {
    name: String,
    method: HttpMethod,
    url: String,
    #[serde(default)]
    body: Option<String>,
    expect_status: u16,
    #[serde(default)]
    body_contains: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

#[derive(serde::Deserialize)]
struct GrpcHealth {
    url: String,
    service: String,
    attempts: u32,
    interval_millis: u64,
}

#[derive(serde::Deserialize)]
struct GrpcAssertion {
    name: String,
    url: String,
    service: String,
    expect_status: GrpcServingStatus,
}

#[derive(serde::Deserialize)]
struct GrpcOperation {
    name: String,
    url: String,
    service: String,
    expect_status: GrpcServingStatus,
}

/// A host sends this JSON after the boot marker for a Compose gRPC-health
/// campaign operation. The service only needs the standard health endpoint.
#[derive(serde::Deserialize)]
struct CampaignGrpcOperation {
    name: String,
    url: String,
    service: String,
    expect_status: GrpcServingStatus,
}

/// An argv command run in the image filesystem. It is intentionally not a
/// shell snippet: arguments are passed to execve unchanged.
#[derive(serde::Deserialize)]
struct ShellOperation {
    name: String,
    command: Vec<String>,
    expect_exit: i32,
    #[serde(default)]
    output_contains: Option<String>,
    #[serde(default)]
    output_json: bool,
    #[serde(default)]
    environment: BTreeMap<String, String>,
}

/// A host sends this JSON after the boot marker for a Compose command
/// operation. The command stays entirely inside the image VM.
#[derive(serde::Deserialize)]
struct CampaignShellOperation {
    name: String,
    command: Vec<String>,
    expect_exit: i32,
    #[serde(default)]
    output_contains: Option<String>,
    #[serde(default)]
    output_json: bool,
    #[serde(default)]
    environment: BTreeMap<String, String>,
}

#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrpcServingStatus {
    Unknown,
    Serving,
    NotServing,
    ServiceUnknown,
}

fn mount(source: &str, target: &str, fstype: &str) {
    let source = CString::new(source).unwrap();
    let target = CString::new(target).unwrap();
    let fstype = CString::new(fstype).unwrap();
    unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        );
    }
}

fn exec_argv(
    spec: &InitSpec,
    command: &[String],
    environment: &BTreeMap<String, String>,
) -> Result<(), String> {
    let program = command
        .first()
        .ok_or_else(|| "command has no program".to_owned())?;
    let argv: Vec<CString> = command
        .iter()
        .map(|a| CString::new(a.as_str()).map_err(|_| "command contains NUL".to_owned()))
        .collect::<Result<_, _>>()?;
    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    let mut entries = spec.env.clone();
    for (key, value) in environment {
        let entry = format!("{key}={value}");
        if let Some(position) = entries.iter().rposition(|candidate| {
            candidate
                .split_once('=')
                .is_some_and(|(name, _)| name == key)
        }) {
            entries[position] = entry;
        } else {
            entries.push(entry);
        }
    }
    entries.retain(|entry| {
        entry
            .split_once('=')
            .is_none_or(|(key, _)| key != "THESEUS_CHANNEL")
    });
    entries.push("THESEUS_CHANNEL=serial:/dev/ttyS0".to_owned());
    let env: Vec<CString> = entries
        .iter()
        .map(|entry| {
            CString::new(entry.as_str()).map_err(|_| "environment contains NUL".to_owned())
        })
        .collect::<Result<_, _>>()?;
    let env_ptrs: Vec<*const libc::c_char> = env
        .iter()
        .map(|e| e.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    if !spec.workdir.is_empty() {
        let workdir = CString::new(spec.workdir.as_str())
            .map_err(|_| "working directory contains NUL".to_owned())?;
        if unsafe { libc::chdir(workdir.as_ptr()) } != 0 {
            return Err(format!(
                "cannot change to working directory {:?}: {}",
                spec.workdir,
                std::io::Error::last_os_error()
            ));
        }
    }

    let paths = executable_paths(program, &entries);
    let mut last_error = None;
    for path in paths {
        let executable =
            CString::new(path.as_str()).map_err(|_| "command contains NUL".to_owned())?;
        let rc = unsafe { libc::execve(executable.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr()) };
        let error = std::io::Error::last_os_error();
        // Docker resolves a bare JSON-form command through PATH. Continue
        // searching only when this directory has no matching executable.
        if error.raw_os_error().is_some_and(|code| code == libc::ENOENT || code == libc::ENOTDIR)
        {
            last_error = Some(error);
            continue;
        }
        return Err(format!(
            "failed to exec {executable:?} (rc={rc}, err={error})"
        ));
    }
    Err(format!(
        "failed to find command {program:?} in PATH: {}",
        last_error.unwrap_or_else(std::io::Error::last_os_error)
    ))
}

fn executable_paths(program: &str, environment: &[String]) -> Vec<String> {
    if program.contains('/') {
        return vec![program.to_owned()];
    }
    let path = environment
        .iter()
        .rev()
        .find_map(|entry| entry.strip_prefix("PATH="))
        .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    path.split(':')
        .map(|directory| {
            if directory.is_empty() {
                program.to_owned()
            } else {
                format!("{directory}/{program}")
            }
        })
        .collect()
}

fn exec_image(spec: &InitSpec) -> ! {
    if let Err(error) = exec_argv(spec, &spec.argv, &BTreeMap::new()) {
        eprintln!("pivot: {error}");
    }
    power_off();
}

struct HttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(value: &str) -> Result<HttpUrl, String> {
    let rest = value
        .strip_prefix("http://")
        .ok_or_else(|| "only http:// URLs are supported".to_owned())?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() || authority.contains('@') || authority.contains('[') {
        return Err("URL must contain a hostname and optional port".to_owned());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| "URL has an invalid port".to_owned())?,
        ),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err("URL has an empty hostname".to_owned());
    }
    Ok(HttpUrl {
        host: host.to_owned(),
        port,
        path: format!("/{path}"),
    })
}

fn http_request(method: &str, url: &str, body: Option<&str>) -> Result<(u16, Vec<u8>), String> {
    let url = parse_http_url(url)?;
    let address = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|error| format!("cannot resolve {}: {error}", url.host))?
        .next()
        .ok_or_else(|| format!("cannot resolve {}", url.host))?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map_err(|error| format!("cannot connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| format!("cannot set read timeout: {error}"))?;
    let body = body.unwrap_or("");
    stream
        .write_all(
            format!(
                "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                url.path,
                url.host,
                body.len(),
            )
            .as_bytes(),
        )
        .and_then(|()| stream.write_all(body.as_bytes()))
        .map_err(|error| format!("cannot send request: {error}"))?;
    let mut response = Vec::new();
    stream
        .take(1024 * 1024)
        .read_to_end(&mut response)
        .map_err(|error| format!("cannot read response: {error}"))?;
    let line_end = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| "response has no HTTP status line".to_owned())?;
    let line = std::str::from_utf8(&response[..line_end])
        .map_err(|_| "response status line is not UTF-8".to_owned())?;
    let status = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "response has no status code".to_owned())?
        .parse::<u16>()
        .map_err(|_| "response has an invalid status code".to_owned())?;
    let body = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|offset| response[offset + 4..].to_vec())
        .unwrap_or_default();
    Ok((status, body))
}

fn http_get(url: &str) -> Result<(u16, Vec<u8>), String> {
    http_request("GET", url, None)
}

fn wait_for_ready(ready: &HttpReady) -> Result<(), String> {
    let mut last_error = "endpoint did not respond".to_owned();
    for attempt in 0..ready.attempts {
        match http_get(&ready.url) {
            Ok((status, _)) if (200..500).contains(&status) => return Ok(()),
            Ok((status, _)) => last_error = format!("endpoint returned HTTP {status}"),
            Err(error) => last_error = error,
        }
        if attempt + 1 < ready.attempts {
            thread::sleep(Duration::from_millis(ready.interval_millis));
        }
    }
    Err(last_error)
}

fn assert_http(assertion: &HttpAssertion) -> Result<(), String> {
    let (status, body) = http_get(&assertion.url)?;
    if status != assertion.expect_status {
        return Err(format!(
            "expected HTTP {}, got HTTP {status}",
            assertion.expect_status
        ));
    }
    if let Some(expected) = &assertion.body_contains {
        if !body
            .windows(expected.len())
            .any(|window| window == expected.as_bytes())
        {
            return Err(format!("response body does not contain {expected:?}"));
        }
    }
    Ok(())
}

fn run_http_operation(operation: &HttpOperation) -> Result<(), String> {
    let method = match operation.method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
    };
    let (status, body) = http_request(method, &operation.url, operation.body.as_deref())?;
    if status != operation.expect_status {
        return Err(format!(
            "expected HTTP {}, got HTTP {status}",
            operation.expect_status
        ));
    }
    if let Some(expected) = &operation.body_contains {
        if !body
            .windows(expected.len())
            .any(|window| window == expected.as_bytes())
        {
            return Err(format!("response body does not contain {expected:?}"));
        }
    }
    Ok(())
}

fn run_campaign_http_operation(operation: CampaignHttpOperation) -> Result<(), String> {
    run_http_operation(&HttpOperation {
        name: operation.name,
        method: operation.method,
        url: operation.url,
        body: operation.body,
        expect_status: operation.expect_status,
        body_contains: operation.body_contains,
    })
}

fn h2_frame(
    stream: &mut TcpStream,
    kind: u8,
    flags: u8,
    stream_id: u32,
    payload: &[u8],
) -> Result<(), String> {
    if payload.len() > 0x00ff_ffff {
        return Err("HTTP/2 frame is too large".to_owned());
    }
    let length = payload.len() as u32;
    stream
        .write_all(&[
            (length >> 16) as u8,
            (length >> 8) as u8,
            length as u8,
            kind,
            flags,
            ((stream_id >> 24) & 0x7f) as u8,
            (stream_id >> 16) as u8,
            (stream_id >> 8) as u8,
            stream_id as u8,
        ])
        .and_then(|()| stream.write_all(payload))
        .map_err(|error| format!("cannot send HTTP/2 frame: {error}"))
}

fn hpack_integer(out: &mut Vec<u8>, value: usize, prefix: u8, first: u8) {
    let maximum = (1usize << prefix) - 1;
    if value < maximum {
        out.push(first | value as u8);
        return;
    }
    out.push(first | maximum as u8);
    let mut remaining = value - maximum;
    while remaining >= 128 {
        out.push((remaining as u8 & 0x7f) | 0x80);
        remaining >>= 7;
    }
    out.push(remaining as u8);
}

fn hpack_string(out: &mut Vec<u8>, value: &str) -> Result<(), String> {
    if value.len() >= 127 {
        return Err("gRPC URL or service name is too long".to_owned());
    }
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn hpack_literal(out: &mut Vec<u8>, name_index: usize, value: &str) -> Result<(), String> {
    // Literal header without indexing. The static table supplies the name.
    hpack_integer(out, name_index, 4, 0);
    hpack_string(out, value)
}

fn grpc_headers(authority: &str) -> Result<Vec<u8>, String> {
    let mut headers = Vec::new();
    // :method POST and :scheme http are fully indexed in HPACK's static table.
    headers.extend_from_slice(&[0x83, 0x86]);
    hpack_literal(&mut headers, 4, "/grpc.health.v1.Health/Check")?;
    hpack_literal(&mut headers, 1, authority)?;
    hpack_literal(&mut headers, 31, "application/grpc")?;
    hpack_literal(&mut headers, 57, "trailers")?;
    Ok(headers)
}

fn protobuf_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn grpc_health(url: &str, service: &str) -> Result<GrpcServingStatus, String> {
    let url = parse_http_url(url)?;
    let address = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|error| format!("cannot resolve {}: {error}", url.host))?
        .next()
        .ok_or_else(|| format!("cannot resolve {}", url.host))?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map_err(|error| format!("cannot connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(1))))
        .map_err(|error| format!("cannot configure connection: {error}"))?;
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .map_err(|error| format!("cannot send HTTP/2 preface: {error}"))?;
    h2_frame(&mut stream, 4, 0, 0, &[])?;
    let authority = format!("{}:{}", url.host, url.port);
    h2_frame(&mut stream, 1, 0x4, 1, &grpc_headers(&authority)?)?;
    let mut message = Vec::with_capacity(service.len() + 2);
    if !service.is_empty() {
        message.push(0x0a);
        protobuf_varint(&mut message, service.len());
        message.extend_from_slice(service.as_bytes());
    }
    let mut request = Vec::with_capacity(message.len() + 5);
    request.push(0);
    request.extend_from_slice(&(message.len() as u32).to_be_bytes());
    request.extend_from_slice(&message);
    h2_frame(&mut stream, 0, 0x1, 1, &request)?;

    let mut grpc_payload = Vec::new();
    loop {
        let mut header = [0u8; 9];
        stream
            .read_exact(&mut header)
            .map_err(|error| format!("cannot read HTTP/2 frame: {error}"))?;
        let length =
            ((header[0] as usize) << 16) | ((header[1] as usize) << 8) | header[2] as usize;
        let kind = header[3];
        let flags = header[4];
        let stream_id = u32::from_be_bytes([header[5] & 0x7f, header[6], header[7], header[8]]);
        let mut payload = vec![0; length];
        stream
            .read_exact(&mut payload)
            .map_err(|error| format!("cannot read HTTP/2 frame body: {error}"))?;
        match kind {
            4 if flags & 0x1 == 0 => h2_frame(&mut stream, 4, 0x1, 0, &[])?,
            0 if stream_id == 1 => {
                grpc_payload.extend_from_slice(&payload);
                if flags & 0x1 != 0 {
                    break;
                }
            }
            1 if stream_id == 1 && flags & 0x1 != 0 => break,
            3 if stream_id == 1 => return Err("gRPC server reset the health request".to_owned()),
            _ => {}
        }
    }
    if grpc_payload.len() < 7 || grpc_payload[0] != 0 {
        return Err("gRPC health response has no uncompressed message".to_owned());
    }
    let length = u32::from_be_bytes(grpc_payload[1..5].try_into().unwrap()) as usize;
    let message = grpc_payload
        .get(5..5 + length)
        .ok_or_else(|| "gRPC health response is truncated".to_owned())?;
    if message.len() < 2 || message[0] != 0x08 {
        return Err("gRPC health response has no serving status".to_owned());
    }
    match message[1] {
        0 => Ok(GrpcServingStatus::Unknown),
        1 => Ok(GrpcServingStatus::Serving),
        2 => Ok(GrpcServingStatus::NotServing),
        3 => Ok(GrpcServingStatus::ServiceUnknown),
        status => Err(format!("gRPC health response has unknown status {status}")),
    }
}

fn wait_for_grpc_ready(ready: &GrpcHealth) -> Result<(), String> {
    let mut last_error = "endpoint did not respond".to_owned();
    for attempt in 0..ready.attempts {
        match grpc_health(&ready.url, &ready.service) {
            Ok(GrpcServingStatus::Serving) => return Ok(()),
            Ok(status) => last_error = format!("health status is {status:?}"),
            Err(error) => last_error = error,
        }
        if attempt + 1 < ready.attempts {
            thread::sleep(Duration::from_millis(ready.interval_millis));
        }
    }
    Err(last_error)
}

fn assert_grpc(assertion: &GrpcAssertion) -> Result<(), String> {
    let actual = grpc_health(&assertion.url, &assertion.service)?;
    if actual == assertion.expect_status {
        Ok(())
    } else {
        Err(format!(
            "expected {:?}, got {actual:?}",
            assertion.expect_status
        ))
    }
}

fn run_grpc_operation(operation: &GrpcOperation) -> Result<(), String> {
    let actual = grpc_health(&operation.url, &operation.service)?;
    if actual == operation.expect_status {
        Ok(())
    } else {
        Err(format!(
            "expected {:?}, got {actual:?}",
            operation.expect_status
        ))
    }
}

fn run_campaign_grpc_operation(operation: CampaignGrpcOperation) -> Result<(), String> {
    run_grpc_operation(&GrpcOperation {
        name: operation.name,
        url: operation.url,
        service: operation.service,
        expect_status: operation.expect_status,
    })
}

const SHELL_OUTPUT_LIMIT: usize = 64 * 1024;

struct ShellOperationResult {
    output_json: Option<serde_json::Value>,
}

fn run_shell_operation(
    spec: &InitSpec,
    operation: &ShellOperation,
) -> Result<ShellOperationResult, String> {
    let mut pipe_fds = [0; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        return Err(format!(
            "cannot create command output pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }
        return Err(format!(
            "cannot start command: {}",
            std::io::Error::last_os_error()
        ));
    }
    if pid == 0 {
        unsafe {
            libc::close(pipe_fds[0]);
            libc::dup2(pipe_fds[1], libc::STDOUT_FILENO);
            libc::dup2(pipe_fds[1], libc::STDERR_FILENO);
            libc::close(pipe_fds[1]);
        }
        if let Err(error) = exec_argv(spec, &operation.command, &operation.environment) {
            eprintln!("pivot: {error}");
        }
        std::process::exit(127);
    }

    unsafe { libc::close(pipe_fds[1]) };
    let mut output = Vec::new();
    let mut output_truncated = false;
    let mut reader = unsafe { std::fs::File::from_raw_fd(pipe_fds[0]) };
    let mut buffer = [0; 4096];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("cannot read command output: {error}"))?;
        if count == 0 {
            break;
        }
        let remaining = SHELL_OUTPUT_LIMIT.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
        output_truncated |= count > remaining;
    }

    let mut status = 0;
    if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
        return Err(format!(
            "cannot wait for command: {}",
            std::io::Error::last_os_error()
        ));
    }
    if !libc::WIFEXITED(status) {
        return Err(format!(
            "command was terminated by signal {}",
            libc::WTERMSIG(status)
        ));
    }
    let actual_exit = libc::WEXITSTATUS(status);
    if actual_exit != operation.expect_exit {
        return Err(format!(
            "expected exit {}, got {actual_exit}",
            operation.expect_exit
        ));
    }
    if let Some(expected) = &operation.output_contains {
        if !output
            .windows(expected.len())
            .any(|window| window == expected.as_bytes())
        {
            return Err(format!("command output does not contain {expected:?}"));
        }
    }
    let output_json = if operation.output_json {
        if output_truncated {
            return Err("command output exceeded 65536-byte JSON limit".to_owned());
        }
        Some(
            serde_json::from_slice(&output)
                .map_err(|error| format!("command output is not JSON: {error}"))?,
        )
    } else {
        None
    };
    Ok(ShellOperationResult { output_json })
}

fn run_campaign_shell_operation(
    spec: &InitSpec,
    operation: CampaignShellOperation,
) -> Result<ShellOperationResult, String> {
    run_shell_operation(
        spec,
        &ShellOperation {
            name: operation.name,
            command: operation.command,
            expect_exit: operation.expect_exit,
            output_contains: operation.output_contains,
            output_json: operation.output_json,
            environment: operation.environment,
        },
    )
}

fn report_shell_operation(name: &str, result: ShellOperationResult) {
    if let Some(output) = result.output_json {
        println!(
            "{}",
            serde_json::json!({
                "event": "shell_operation",
                "name": name,
                "output": output,
            })
        );
    }
    println!("THES:SHELL:operation:{name}:PASS");
}

fn stop_service(pid: libc::pid_t) {
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    for _ in 0..20 {
        let status = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        if status == pid || status == -1 {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
}

fn power_off() -> ! {
    unsafe {
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    std::process::exit(0);
}

// Linux `ifreq` is a 16-byte interface name followed by a 24-byte union on
// the x86_64 and aarch64 guests Theseus publishes. Keeping the request local
// avoids depending on `ip` or a DHCP client in the image being tested.
#[repr(C)]
struct IfReq {
    name: [u8; 16],
    data: [u8; 24],
}

fn ipv4(value: &str) -> Result<[u8; 4], String> {
    let parts = value
        .split('.')
        .map(|part| {
            part.parse::<u8>()
                .map_err(|_| format!("invalid IPv4 address {value:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    parts
        .try_into()
        .map_err(|_| format!("invalid IPv4 address {value:?}"))
}

fn ifreq(name: &str) -> Result<IfReq, String> {
    if name.is_empty() || name.len() >= 16 || !name.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(format!("invalid interface name {name:?}"));
    }
    let mut request = IfReq {
        name: [0; 16],
        data: [0; 24],
    };
    request.name[..name.len()].copy_from_slice(name.as_bytes());
    Ok(request)
}

fn set_sockaddr(request: &mut IfReq, address: [u8; 4]) {
    request.data = [0; 24];
    request.data[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
    request.data[4..8].copy_from_slice(&address);
}

fn ioctl(fd: libc::c_int, command: libc::Ioctl, request: &mut IfReq) -> Result<(), String> {
    if unsafe { libc::ioctl(fd, command, request) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn configure_interface(interface: &NetworkInterface) -> Result<(), String> {
    if interface.prefix_len > 32 {
        return Err(format!(
            "invalid IPv4 prefix length {}",
            interface.prefix_len
        ));
    }
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(format!(
            "cannot open network control socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    let result = (|| {
        let mut request = ifreq(&interface.name)?;
        set_sockaddr(&mut request, ipv4(&interface.address)?);
        ioctl(fd, libc::SIOCSIFADDR as libc::Ioctl, &mut request)?;

        let mask = if interface.prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - interface.prefix_len)
        };
        let mut request = ifreq(&interface.name)?;
        set_sockaddr(&mut request, mask.to_be_bytes());
        ioctl(fd, libc::SIOCSIFNETMASK as libc::Ioctl, &mut request)?;

        let mut request = ifreq(&interface.name)?;
        ioctl(fd, libc::SIOCGIFFLAGS as libc::Ioctl, &mut request)?;
        let flags = i16::from_ne_bytes([request.data[0], request.data[1]]) | (libc::IFF_UP as i16);
        request.data[..2].copy_from_slice(&flags.to_ne_bytes());
        ioctl(fd, libc::SIOCSIFFLAGS as libc::Ioctl, &mut request)
    })();
    unsafe { libc::close(fd) };
    result.map_err(|error| format!("{}: {error}", interface.name))
}

fn configure_network(network: &ContainerNetwork) -> Result<(), String> {
    for interface in &network.interfaces {
        configure_interface(interface)?;
    }
    if network.hosts.is_empty() {
        return Ok(());
    }
    let mut hosts = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/etc/hosts")
        .map_err(|error| format!("cannot open /etc/hosts: {error}"))?;
    for (name, address) in &network.hosts {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(format!("invalid host name {name:?}"));
        }
        writeln!(hosts, "{address} {name}")
            .map_err(|error| format!("cannot write /etc/hosts: {error}"))?;
    }
    Ok(())
}

fn main() {
    mount("devtmpfs", "/dev", "devtmpfs");
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");

    let spec: InitSpec = serde_json::from_str(
        &fs::read_to_string("/etc/theseus-init.json").expect("read theseus-init.json"),
    )
    .expect("parse theseus-init.json");

    // `service.network` is retained as a compatibility fallback for replay
    // bundles written before network setup became independent of service
    // checks. New images always use the top-level contract.
    let network = spec
        .network
        .as_ref()
        .filter(|network| !network.is_empty())
        .or_else(|| {
            spec.container_service
                .as_ref()
                .map(|service| &service.network)
                .filter(|network| !network.is_empty())
        });
    if let Some(network) = network {
        if let Err(error) = configure_network(network) {
            eprintln!("THES:network:FAIL {error}");
            power_off();
        }
    }
    let mut channel = TtyChannel::console().expect("open /dev/ttyS0");
    let Some(service) = spec.container_service.as_ref() else {
        channel.marker(MARKER_BOOT).expect("boot marker");
        exec_image(&spec);
    };

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        exec_image(&spec);
    }
    if pid < 0 {
        eprintln!("THES:HTTP:ready:FAIL could not start service");
        power_off();
    }

    if let Some(ready) = &service.ready {
        match wait_for_ready(ready) {
            Ok(()) => println!("THES:HTTP:ready:PASS"),
            Err(error) => {
                eprintln!("THES:HTTP:ready:FAIL {error}");
                stop_service(pid);
                power_off();
            }
        }
    }
    if let Some(ready) = &service.grpc_ready {
        match wait_for_grpc_ready(ready) {
            Ok(()) => println!("THES:GRPC:ready:PASS"),
            Err(error) => {
                eprintln!("THES:GRPC:ready:FAIL {error}");
                stop_service(pid);
                power_off();
            }
        }
    }
    channel.marker(MARKER_BOOT).expect("boot marker");
    if service.campaign {
        loop {
            let (protocol, command) = match channel.next_command_any(&[
                "THES:HTTP:operation:",
                "THES:GRPC:operation:",
                "THES:SHELL:operation:",
            ]) {
                Ok(command) => command,
                Err(error) => {
                    eprintln!("THES:operation:FAIL cannot read campaign command: {error}");
                    stop_service(pid);
                    power_off();
                }
            };
            if protocol == 0 {
                let operation: CampaignHttpOperation = match serde_json::from_str(&command) {
                    Ok(operation) => operation,
                    Err(error) => {
                        eprintln!("THES:HTTP:operation:FAIL invalid campaign command: {error}");
                        continue;
                    }
                };
                let name = operation.name.clone();
                match run_campaign_http_operation(operation) {
                    Ok(()) => println!("THES:HTTP:operation:{name}:PASS"),
                    Err(error) => eprintln!("THES:HTTP:operation:{name}:FAIL {error}"),
                }
                channel
                    .checkpoint(&name)
                    .expect("campaign operation checkpoint");
            } else if protocol == 1 {
                let operation: CampaignGrpcOperation = match serde_json::from_str(&command) {
                    Ok(operation) => operation,
                    Err(error) => {
                        eprintln!("THES:GRPC:operation:FAIL invalid campaign command: {error}");
                        continue;
                    }
                };
                let name = operation.name.clone();
                match run_campaign_grpc_operation(operation) {
                    Ok(()) => println!("THES:GRPC:operation:{name}:PASS"),
                    Err(error) => eprintln!("THES:GRPC:operation:{name}:FAIL {error}"),
                }
                channel
                    .checkpoint(&name)
                    .expect("campaign operation checkpoint");
            } else {
                let operation: CampaignShellOperation = match serde_json::from_str(&command) {
                    Ok(operation) => operation,
                    Err(error) => {
                        eprintln!("THES:SHELL:operation:FAIL invalid campaign command: {error}");
                        continue;
                    }
                };
                let name = operation.name.clone();
                match run_campaign_shell_operation(&spec, operation) {
                    Ok(result) => report_shell_operation(&name, result),
                    Err(error) => eprintln!("THES:SHELL:operation:{name}:FAIL {error}"),
                }
                channel
                    .checkpoint(&name)
                    .expect("campaign operation checkpoint");
            }
        }
    }
    for operation in &service.operations {
        match run_http_operation(operation) {
            Ok(()) => println!("THES:HTTP:operation:{}:PASS", operation.name),
            Err(error) => eprintln!("THES:HTTP:operation:{}:FAIL {error}", operation.name),
        }
    }
    for assertion in &service.assertions {
        match assert_http(assertion) {
            Ok(()) => println!("THES:HTTP:{}:PASS", assertion.name),
            Err(error) => eprintln!("THES:HTTP:{}:FAIL {error}", assertion.name),
        }
    }
    for assertion in &service.grpc_assertions {
        match assert_grpc(assertion) {
            Ok(()) => println!("THES:GRPC:{}:PASS", assertion.name),
            Err(error) => eprintln!("THES:GRPC:{}:FAIL {error}", assertion.name),
        }
    }
    for operation in &service.grpc_operations {
        match run_grpc_operation(operation) {
            Ok(()) => println!("THES:GRPC:operation:{}:PASS", operation.name),
            Err(error) => eprintln!("THES:GRPC:operation:{}:FAIL {error}", operation.name),
        }
    }
    for operation in &service.shell_operations {
        match run_shell_operation(&spec, operation) {
            Ok(result) => report_shell_operation(&operation.name, result),
            Err(error) => eprintln!("THES:SHELL:operation:{}:FAIL {error}", operation.name),
        }
    }
    stop_service(pid);
    power_off();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_bare_image_command_with_the_locked_path() {
        assert_eq!(
            executable_paths("httpd", &["PATH=/custom/bin:/bin".to_owned()]),
            ["/custom/bin/httpd", "/bin/httpd"]
        );
        assert_eq!(
            executable_paths("/bin/httpd", &[]),
            ["/bin/httpd"]
        );
    }
}
