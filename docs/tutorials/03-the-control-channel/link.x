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
