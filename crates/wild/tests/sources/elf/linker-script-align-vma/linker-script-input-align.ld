ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
}

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    } :text

    . = ALIGN(8);
    .exit.text : {
        KEEP(*(.exit.text))
    }

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .data .data.* .bss .bss.*)
    }
}

ASSERT((ADDR(.exit.text) & 15) == 0, ".exit.text is aligned to input sh_addralign after ALIGN(8)");
