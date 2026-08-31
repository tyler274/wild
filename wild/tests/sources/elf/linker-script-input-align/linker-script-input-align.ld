ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
}

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text.small .text.big))
        KEEP(*(.text .text.*))
    } :text

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .data .data.* .bss .bss.*)
    }
}

ASSERT(ADDR(.text) == 0x400000, ".text starts at the location counter");
