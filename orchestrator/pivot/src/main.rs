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

use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
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
}

#[derive(serde::Deserialize)]
struct ContainerService {
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

fn exec_image(spec: &InitSpec) -> ! {
    let argv: Vec<CString> = spec
        .argv
        .iter()
        .map(|a| CString::new(a.as_str()).unwrap())
        .collect();
    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    let mut env: Vec<CString> = spec
        .env
        .iter()
        .map(|e| CString::new(e.as_str()).unwrap())
        .collect();
    env.push(CString::new("THESEUS_CHANNEL=serial:/dev/ttyS0").unwrap());
    let env_ptrs: Vec<*const libc::c_char> = env
        .iter()
        .map(|e| e.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    if !spec.workdir.is_empty() {
        let workdir = CString::new(spec.workdir.as_str()).unwrap();
        unsafe { libc::chdir(workdir.as_ptr()) };
    }

    let program = CString::new(spec.argv[0].as_str()).unwrap();
    let rc = unsafe { libc::execve(program.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr()) };
    // execve only returns on failure.
    let err = std::io::Error::last_os_error();
    eprintln!(
        "pivot: failed to exec {:?} (rc={rc}, err={err})",
        spec.argv[0]
    );
    unsafe {
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    std::process::exit(127);
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

fn main() {
    mount("devtmpfs", "/dev", "devtmpfs");
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");

    let spec: InitSpec = serde_json::from_str(
        &fs::read_to_string("/etc/theseus-init.json").expect("read theseus-init.json"),
    )
    .expect("parse theseus-init.json");

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
    stop_service(pid);
    power_off();
}
