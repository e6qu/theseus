// Copyright 2026 Theseus contributors.
// SPDX-License-Identifier: Apache-2.0

//! Theseus e2e guest agent: runs as /init in a minimal initramfs on a stock
//! CI kernel. Proves the SDK's Linux transport: dumps seeded entropy, then
//! does a control-channel round trip over the serial console.

use std::fs::File;
use std::io::Read;

use theseus_sdk::linux::TtyChannel;
use theseus_sdk::{EVENT_TERMINATOR, MARKER_BOOT, MARKER_DONE};

/// initramfs devtmpfs population races with driver registration; create the
/// nodes we need ourselves (as the C init did).
fn mknod(path: &str, major: u32, minor: u32) {
    let c_path = std::ffi::CString::new(path).unwrap();
    unsafe {
        libc::mknod(
            c_path.as_ptr(),
            libc::S_IFCHR | 0o444,
            libc::makedev(major, minor),
        );
    }
}

fn dump(label: &str, path: &str) {
    let mut buf = [0u8; 64];
    match File::open(path).and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => {
            let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
            println!("{label} (64 bytes): {hex}");
        }
        Err(err) => println!("{label}: FAILED: {err}"),
    }
}

fn main() {
    println!("theseus-agent init");
    mknod("/dev/hwrng", 10, 183);
    mknod("/dev/urandom", 1, 9);
    mknod("/dev/ttyS0", 4, 64);
    dump("hwrng", "/dev/hwrng");
    dump("urandom", "/dev/urandom");
    println!("entropy done");

    let mut channel = TtyChannel::console().expect("open /dev/ttyS0");
    println!("channel open");
    channel.marker(MARKER_BOOT).unwrap();
    loop {
        let event = channel.next_event().expect("read event");
        if event == EVENT_TERMINATOR {
            break;
        }
        channel.marker(event).unwrap();
    }
    channel.marker(MARKER_DONE).unwrap();

    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
}
