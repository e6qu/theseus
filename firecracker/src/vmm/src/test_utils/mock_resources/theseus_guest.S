// Theseus test guest: a minimal arm64 "kernel" (Image format) that talks to
// the control channel over MMIO and prints to the serial console.
//
// Built by make_theseus_guest.sh into theseus_guest.bin. Loaded at
// DRAM_MEM_START (0x80000000) by the linux-loader PE/Image loader; entry at
// offset 0 with x0 = FDT address, MMU off.
//
// Layout after the Phase-5 slot shift (aarch64):
//   0x40000000  boot timer
//   0x40001000  RTC
//   0x40002000  serial (UART)
//   0x40003000  theseus control device

    .text
_head:
    // arm64 Image header (64 bytes)
    b       _start                  // code0
    .word   0                       // code1
    .quad   0                       // text_offset
    .quad   _end - _head            // image_size
    .quad   0                       // flags: little-endian
    .quad   0                       // res2
    .quad   0                       // res3
    .quad   0                       // res4
    .ascii  "ARM\x64"               // magic
    .word   0                       // res5

_start:
    ldr     x9, =0x40002000         // UART
    ldr     x10, =0x40003000        // theseus control device

    // banner
    adr     x11, msg_banner
1:  ldrb    w12, [x11], #1
    cbz     w12, 2f
    strb    w12, [x9]
    b       1b

2:  // read the magic register byte by byte and echo it
    ldrb    w12, [x10]
    strb    w12, [x9]
    ldrb    w12, [x10, #1]
    strb    w12, [x9]
    ldrb    w12, [x10, #2]
    strb    w12, [x9]
    ldrb    w12, [x10, #3]
    strb    w12, [x9]

    // read status (offset 4), ignore value
    ldrb    w12, [x10, #4]

    // print the guest virtual counter (CNTVCT) as 16 hex digits
    adr     x11, msg_vtime
10: ldrb    w12, [x11], #1
    cbz     w12, 11f
    strb    w12, [x9]
    b       10b
11: mrs     x13, cntvct_el0
    mov     x14, #60                // bit shift, MSB nibble first
12: lsr     x15, x13, x14
    and     x15, x15, #0xf
    cmp     x15, #10
    b.lo    13f
    add     x15, x15, #('a' - '0' - 10)
13: add     x15, x15, #'0'
    strb    w15, [x9]
    subs    x14, x14, #4
    b.ge    12b

    // log marker 0x42 (offset 7): "booted"
    mov     w12, #0x42
    strb    w12, [x10, #7]

    // command: setup complete (offset 6)
    mov     w12, #1
    strb    w12, [x10, #6]

    // echo loop: wait for host events, log each back; 0x00 terminates the
    // round (not the program — the guest loops back to waiting, so timelines
    // branched from a pause between rounds resume into the wait state).
20: ldrb    w12, [x10, #4]          // status
    tst     w12, #1                 // events pending?
    b.eq    20b
    ldrb    w12, [x10, #5]          // pop event
    cbz     w12, 21f                // terminator
    strb    w12, [x10, #7]          // echo as log marker
    b       20b
21: mov     w12, #0xFF              // done marker
    strb    w12, [x10, #7]
    b       20b                     // wait for the next round

msg_banner: .asciz "guest: magic="
msg_vtime:  .asciz " vtime=0x"

_end:
