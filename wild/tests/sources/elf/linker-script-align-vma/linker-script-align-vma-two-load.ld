ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    } :text

    . = ALIGN(0x200000);
    .data : {
        KEEP(*(.data .data.*))
    } :data

    __global_pointer$ = . + 0x800;

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr)
    }
}

ASSERT(ADDR(.data) == 0x600000, "ALIGN before a new PT_LOAD is the LOAD VMA");
