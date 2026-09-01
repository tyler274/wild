ENTRY(_start)

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    }
    .data : {
        KEEP(*(.data .data.*))
    }
    /DISCARD/ : {
        *(.discard)
    }
}
