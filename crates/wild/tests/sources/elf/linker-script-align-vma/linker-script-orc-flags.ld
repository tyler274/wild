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

    .data : {
        KEEP(*(.data .data.*))
    } :data

    .orc_lookup : {
        . += 16;
    }

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .bss .bss.*)
    }
}
