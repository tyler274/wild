ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    note PT_NOTE FLAGS(4);
}

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    } :text

    .notes : {
        KEEP(*(.note.*))
    } :text :note

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr .data .data.*)
    }
}

ASSERT(SIZEOF(.notes) >= 0x14, "linker --build-id is merged into .notes");
