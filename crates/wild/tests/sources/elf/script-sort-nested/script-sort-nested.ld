ENTRY(_start)

SECTIONS {
    .text : { *(.text .text.*) }
    .name_then_align : {
        *(SORT_BY_NAME(SORT_BY_ALIGNMENT(.nta.*)))
    }
    .align_then_name : {
        *(SORT_BY_ALIGNMENT(SORT_BY_NAME(.atn.*)))
    }
    /DISCARD/ : { *(.comment .note* .eh_frame* .rodata*) }
}
