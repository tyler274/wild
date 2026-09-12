ENTRY(_start)

SECTIONS {
    .text : { *(.text .text.*) }
    .keep : {
        *(SORT_BY_NAME(EXCLUDE_FILE(*script-sort-exclude-2*.o) .sorted.*))
    }
    .drop : {
        *(.sorted.*)
    }
    /DISCARD/ : { *(.comment .note* .eh_frame* .rodata*) }
}
