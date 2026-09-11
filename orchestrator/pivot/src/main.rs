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
    ready: HttpReady,
    #[serde(default)]
    assertions: Vec<HttpAssertion>,
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

fn http_get(url: &str) -> Result<(u16, Vec<u8>), String> {
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
    stream
        .write_all(
            format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                url.path, url.host
            )
            .as_bytes(),
        )
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

    match wait_for_ready(&service.ready) {
        Ok(()) => {
            println!("THES:HTTP:ready:PASS");
            channel.marker(MARKER_BOOT).expect("boot marker");
        }
        Err(error) => {
            eprintln!("THES:HTTP:ready:FAIL {error}");
            stop_service(pid);
            power_off();
        }
    }
    for assertion in &service.assertions {
        match assert_http(assertion) {
            Ok(()) => println!("THES:HTTP:{}:PASS", assertion.name),
            Err(error) => eprintln!("THES:HTTP:{}:FAIL {error}", assertion.name),
        }
    }
    stop_service(pid);
    power_off();
}
