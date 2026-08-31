// Copyright 2026 Adrian Mârza (https://www.linkedin.com/in/adrian-m%C3%A2rza-52606512a/) and contributors to Theseus
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tutorial workload: a replicated counter with a retry bug.
//!
//! Models a two-node protocol inside one bare-metal guest: node A sends
//! increment commands and retries on a lost ack; node B applies every send
//! it sees. The partition event (0xEE) "loses" the next ack, so A retries —
//! and B, seeing the retry as a new send, applies the command a second
//! time. That is the at-least-once-vs-exactly-once bug this tutorial hunts.
//!
//! Markers: 0x01 = applied once, 0x02 = duplicate application (the bug),
//! 0x42 = boot, 0xFF = round done.

#![no_std]
#![no_main]

use core::arch::global_asm;

use theseus_sdk::{
    CMD_SETUP_COMPLETE, ControlChannel, EVENT_TERMINATOR, MARKER_BOOT, MARKER_DONE,
};

global_asm!(
    r#"
.section .text.header, "ax"
.global _start
_head:
    b _start
    .word 0
    .quad 0
    .quad _end - _head
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .ascii "ARM\x64"
    .word 0
"#
);

const DOOR: *mut u8 = 0x4000_3000 as *mut u8;
const UART: *mut u8 = 0x4000_2000 as *mut u8;

/// The "partition" event: the next ack is lost, so the last command is retried.
const PARTITION: u8 = 0xEE;
/// Marker: command applied (first time).
const MARK_APPLIED: u8 = 0x01;
/// Marker: duplicate application (the bug).
const MARK_DUP: u8 = 0x02;

fn puts(s: &str) {
    for &b in s.as_bytes() {
        unsafe { UART.write_volatile(b) };
    }
}

/// Node B's apply: deliberately non-idempotent (the bug).
fn apply(applied: &mut u32, id: u8, door: &ControlChannel) {
    let bit = 1u32 << (id & 31);
    if *applied & bit != 0 {
        door.marker(MARK_DUP);
    } else {
        *applied |= bit;
        door.marker(MARK_APPLIED);
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    puts("counter-guest\n");
    let door = unsafe { ControlChannel::new(DOOR) };
    if !door.detect() {
        puts("NO DOOR\n");
        loop {}
    }

    door.marker(MARKER_BOOT);
    door.command(CMD_SETUP_COMPLETE);

    let mut applied: u32 = 0;
    let mut last_command: u8 = 0;

    loop {
        door.wait_events();
        let event = door.pop_event();
        match event {
            EVENT_TERMINATOR => door.marker(MARKER_DONE),
            PARTITION => {
                // The ack for `last_command` was lost: node A retries.
                // Node B sees the retry as a new send and applies again.
                apply(&mut applied, last_command, &door);
            }
            command => {
                apply(&mut applied, command, &door);
                last_command = command;
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
