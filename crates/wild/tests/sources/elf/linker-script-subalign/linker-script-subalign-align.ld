ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
}

SECTIONS {
    .s 0x400000 : ALIGN(32) SUBALIGN(4) {
        KEEP(*(.s))
    } :text

    .text : {
        KEEP(*(.text .text.*))
    } :text

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .data .data.* .bss .bss.*)
    }
}
