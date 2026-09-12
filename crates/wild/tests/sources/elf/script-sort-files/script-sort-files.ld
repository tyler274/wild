ENTRY(_start)

SECTIONS {
    .text : { *(.text .text.*) }
    .sort : {
        SORT(*)(.keep.sort)
    }
    .rev : {
        REVERSE(*)(.keep.rev)
    }
    /DISCARD/ : { *(.comment .note* .eh_frame* .rodata*) }
}
