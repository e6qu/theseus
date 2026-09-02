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
const UART: *mut u8 = 0x4000_2000 as *mut u8;
const UART_LINE_STATUS: *const u8 = 0x4000_2005 as *const u8;
const UART_DATA_READY: u8 = 1;
const MARKER_UART_OK: u8 = 0xA1;
const MARKER_UART_ERROR: u8 = 0xEE;

fn read_uart_byte() -> u8 {
    loop {
        // SAFETY: the Firecracker aarch64 UART is mapped at this platform slot.
        if unsafe { UART_LINE_STATUS.read_volatile() } & UART_DATA_READY != 0 {
            // SAFETY: the line-status register reports one byte in the UART FIFO.
            return unsafe { UART.read_volatile() };
        }
    }
}

#[no_mangle]
extern "C" fn _start() -> ! {
    let door = unsafe { ControlChannel::new(DOOR) };
    if door.detect() {
        door.marker(MARKER_BOOT);
        door.command(CMD_SETUP_COMPLETE);
    }
    if read_uart_byte() == b'A' {
        door.marker(MARKER_UART_OK);
    } else {
        door.marker(MARKER_UART_ERROR);
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
