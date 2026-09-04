ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
}

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    } :text

    /DISCARD/ : {
        *(.note .note.* .eh_frame .eh_frame_hdr .data .data.*)
    }
}
