// Copyright 2026 Theseus contributors.
// SPDX-License-Identifier: Apache-2.0

//! Bare-metal test guest written in Rust against theseus-sdk.
//!
//! Behavior: detect the control channel, boot marker, setup-complete, then
//! event rounds forever. Each event takes one of two distinguishable paths
//! (high: >= 0x80, low: < 0x80), echoed back as markers — so different input
//! schedules produce different observable behavior *and* different executed
//! code paths.

#![no_std]
#![no_main]

use core::arch::global_asm;

use theseus_sdk::{
    CMD_SETUP_COMPLETE, ControlChannel, EVENT_TERMINATOR, MARKER_BOOT, MARKER_DONE,
};

// arm64 Image header + entry point. The loader enters at the image base
// (DRAM start); code0 branches to _start.
global_asm!(
    r#"
.section .text.header, "ax"
.global _start
_head:
    b _start
    .word 0
    .quad 0                 // text_offset
    .quad _end - _head      // image_size (must be nonzero for the loader)
    .quad 0                 // flags: little-endian
    .quad 0
    .quad 0
    .quad 0
    .ascii "ARM\x64"
    .word 0
"#
);

/// Serial console (aarch64 platform slot).
const UART: *mut u8 = 0x4000_2000 as *mut u8;
/// Theseus control device (aarch64 platform slot).
const DOOR: *mut u8 = 0x4000_3000 as *mut u8;

/// Marker emitted for high-path events (>= 0x80).
const MARKER_PATH_HIGH: u8 = 0xB0;
/// Marker emitted for low-path events (< 0x80).
const MARKER_PATH_LOW: u8 = 0x50;

fn puts(s: &str) {
    for &b in s.as_bytes() {
        // SAFETY: UART is the platform serial MMIO slot.
        unsafe { UART.write_volatile(b) };
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("guest-rs: ");
    // SAFETY: DOOR is the platform Theseus MMIO slot.
    let door = unsafe { ControlChannel::new(DOOR) };
    if door.detect() {
        puts("THES\n");
    } else {
        puts("NO-DOOR\n");
    }

    door.marker(MARKER_BOOT);
    door.command(CMD_SETUP_COMPLETE);

    loop {
        door.wait_events();
        let event = door.pop_event();
        if event == EVENT_TERMINATOR {
            door.marker(MARKER_DONE);
            continue;
        }
        puts("ev=0x");
        let hex = b"0123456789abcdef";
        unsafe {
            UART.write_volatile(hex[(event >> 4) as usize]);
            UART.write_volatile(hex[(event & 0xf) as usize]);
            UART.write_volatile(b' ');
        }
        if event >= 0x80 {
            door.marker(MARKER_PATH_HIGH);
        } else {
            door.marker(MARKER_PATH_LOW);
        }
        door.marker(event);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    puts("PANIC\n");
    loop {}
}
