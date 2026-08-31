// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pivot: the PID-1 init injected into container-image VMs.
//!
//! It mounts the essentials, reads /etc/theseus-init.json (written by the
//! image flattener), reports a boot marker over the serial control
//! channel, then execs the container image's entrypoint. The image needs
//! no Theseus code of its own — the pivot is the instrumentation.

use std::ffi::CString;
use std::fs;

use theseus_sdk::linux::TtyChannel;
use theseus_sdk::MARKER_BOOT;

#[derive(serde::Deserialize)]
struct InitSpec {
    argv: Vec<String>,
    env: Vec<String>,
    workdir: String,
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

fn main() {
    mount("devtmpfs", "/dev", "devtmpfs");
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");

    let spec: InitSpec = serde_json::from_str(
        &fs::read_to_string("/etc/theseus-init.json").expect("read theseus-init.json"),
    )
    .expect("parse theseus-init.json");

    let mut channel = TtyChannel::console().expect("open /dev/ttyS0");
    channel.marker(MARKER_BOOT).expect("boot marker");

    let argv: Vec<CString> = spec
        .argv
        .iter()
        .map(|a| CString::new(a.as_str()).unwrap())
        .collect();
    let argv_ptrs: Vec<*const libc::c_char> =
        argv.iter().map(|a| a.as_ptr()).chain(std::iter::once(std::ptr::null())).collect();

    let mut env: Vec<CString> = spec
        .env
        .iter()
        .map(|e| CString::new(e.as_str()).unwrap())
        .collect();
    env.push(CString::new("THESEUS_CHANNEL=serial:/dev/ttyS0").unwrap());
    let env_ptrs: Vec<*const libc::c_char> =
        env.iter().map(|e| e.as_ptr()).chain(std::iter::once(std::ptr::null())).collect();

    if !spec.workdir.is_empty() {
        let workdir = CString::new(spec.workdir.as_str()).unwrap();
        unsafe { libc::chdir(workdir.as_ptr()) };
    }

    let program = CString::new(spec.argv[0].as_str()).unwrap();
    let rc = unsafe { libc::execve(program.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr()) };
    // execve only returns on failure.
    let err = std::io::Error::last_os_error();
    eprintln!("pivot: failed to exec {:?} (rc={rc}, err={err})", spec.argv[0]);
    unsafe {
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
}
