/* Link the guest flat at the aarch64 DRAM start; header section first. */
SECTIONS {
    . = 0x80000000;
    .text : {
        KEEP(*(.text.header))
        *(.text*)
    }
    .rodata : { *(.rodata*) }
    .data : { *(.data*) }
    .bss : { *(.bss*) }
    /DISCARD/ : { *(.eh_frame*) *(.comment) }
    _end = .;
}
