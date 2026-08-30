/* Kernel-like vmlinux fragments covering remaining script features. */

ENTRY(_start)

SECTIONS {
    . = 0x1000000;
    _stext = .;
    .text : {
        KEEP(*(.text .text.*))
        *(EXCLUDE_FILE(*discard*.o) .init.text)
    }

    .init_array : {
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*) SORT_BY_INIT_PRIORITY(.ctors.*)))
        KEEP(*(.init_array .ctors))
    }

    .data : AT(ADDR(.data) - 0) {
        KEEP(*(.data .data.*))
        BYTE(0)
    }

    __init_begin = .;
    .init.text : { *(.init.text) }
    __init_end = .;

    __global_pointer$ = . + 0x800;

    /DISCARD/ : {
        *(.discard .discard.*)
    }
}

ASSERT(_stext == 0x1000000, "_stext");
