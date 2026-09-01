ENTRY(_start)

PHDRS {
    load PT_LOAD FLAGS(6);
}

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    } :load
    .before : {
        KEEP(*(.before))
    }
    . = ALIGN(0x1000);
    .init.end : {
        __init_end = .;
    }
    .after : {
        . = ALIGN(0x1000);
        KEEP(*(.after))
        . = ALIGN(0x1000);
    }

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .data .data.* .bss .bss.* .rodata*)
    }
}

ASSERT(ADDR(.after) == 0x401000, ".after VMA follows ALIGN after the empty marker");
ASSERT(SIZEOF(.after) == 0x1000, ".after size includes trailing ALIGN");
