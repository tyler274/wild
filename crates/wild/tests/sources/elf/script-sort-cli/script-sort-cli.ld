ENTRY(_start)

SECTIONS {
    .text : { *(.text .text.*) }
    .sorted : {
        *(.keep.sort.*)
    }
    .byname : {
        *(SORT_BY_NAME(.keep.byname.*))
    }
    /DISCARD/ : { *(.comment .note* .eh_frame* .rodata*) }
}
