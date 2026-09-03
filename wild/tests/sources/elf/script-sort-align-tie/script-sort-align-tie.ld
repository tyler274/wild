ENTRY(_start)

SECTIONS {
    .text : { *(.text .text.*) }
    .data : {
        *(SORT_BY_ALIGNMENT(.data.align.*))
    }
    /DISCARD/ : { *(.comment .note* .eh_frame* .rodata*) }
}
