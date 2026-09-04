SECTIONS {
    /DISCARD/ : {
        *(.discard)
        *(.discard.*)
    }

    __ksymtab 0 : ALIGN(8) { *(SORT(___ksymtab+*)) }
    .ctors 0 : ALIGN(8) { *(SORT(.ctors.*)) *(.ctors) }
    .init_array 0 : ALIGN(8) { *(SORT_BY_INIT_PRIORITY(.init_array.*)) *(.init_array) }

    .text 0 : {
        *(.text .text.*)
    }

    .data 0 : {
        *(.data .data.*)
        BYTE(0)
    }

    .rodata 0 : {
        *(.rodata .rodata.*)
    }
}
