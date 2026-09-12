SECTIONS {
    .text 0 : { *(.text .text.*) }
    .data 0 : {
        *(.data .data.*)
        BYTE(0x11)
        SHORT(0x2233)
    }
    .rodata 0 : { *(.rodata .rodata.*) }
}
