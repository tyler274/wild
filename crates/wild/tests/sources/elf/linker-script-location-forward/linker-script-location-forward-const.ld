ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = later_const;
    .text : {
        _stext = .;
        KEEP(*(.text .text.*))
    } :text

    .data : {
        KEEP(*(.data .data.*))
    } :data

    __global_pointer$ = . + 0x800;

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .rodata*)
    }
}

later_const = 0x500000;

ASSERT(ADDR(.text) == 0x500000, "later-defined constant sets the location counter");
ASSERT(_stext == 0x500000, "_stext follows later_const");
