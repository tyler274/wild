ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
}

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    } :text

    .bss : {
        KEEP(*(.bss .bss.*))
    }

    . = ALIGN(16);
    _end = .;

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .data .data.*)
    }
}

ASSERT(_end == ALIGN(ADDR(.bss) + SIZEOF(.bss), 16), "_end is ALIGN after .bss");
