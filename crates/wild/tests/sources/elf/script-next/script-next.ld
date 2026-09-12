ENTRY(_start)

SECTIONS {
    . = 0x1234;
    next_at = NEXT(0x1000);
    align_at = ALIGN(0x1000);
    two_arg = ALIGN(0x1234, 0x1000);
    .text : { *(.text .text.*) }
    .data : { *(.data .data.*) }
}
