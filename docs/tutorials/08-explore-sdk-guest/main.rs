#![no_std]
#![no_main]

use core::arch::global_asm;
use theseus_sdk::{CMD_SETUP_COMPLETE, ControlChannel, EVENT_TERMINATOR, MARKER_BOOT, MARKER_DONE};

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

#[no_mangle]
extern "C" fn _start() -> ! {
    let door = unsafe { ControlChannel::new(DOOR) };
    if door.detect() {
        door.marker(MARKER_BOOT);
        door.command(CMD_SETUP_COMPLETE);
    }
    loop {
        door.wait_events();
        let event = door.pop_event();
        if event == EVENT_TERMINATOR {
            door.marker(MARKER_DONE);
        } else {
            door.marker(event);
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
