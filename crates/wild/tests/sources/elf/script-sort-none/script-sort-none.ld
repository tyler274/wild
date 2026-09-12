ENTRY(_start)

SECTIONS {
    .text : { *(.text .text.*) }
    .sorted : {
        *(.keep.sort.*)
    }
    .none : {
        *(SORT_NONE(.keep.none.*))
    }
    /DISCARD/ : { *(.comment .note* .eh_frame* .rodata*) }
}
