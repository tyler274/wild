ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = later_p;
    .text : {
        _stext = .;
        KEEP(*(.text .text.*))
    } :text

    .data : {
        KEEP(*(.data .data.*))
    } :data

    PROVIDE(later_p = 0x500000);

    __global_pointer$ = . + 0x800;

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .rodata*)
    }
}

ASSERT(ADDR(.text) == 0x500000, "later PROVIDE constant sets the location counter");
ASSERT(_stext == 0x500000, "_stext follows later_p");
