ENTRY(_start)

SECTIONS {
    .text 0x1000 : {
        KEEP(*(.text.anchor))
        . = alias_sym | 0x100;
        KEEP(*(.text.payload))
        KEEP(*(.text .text.*))
    }

    .data 0x2000 : {
        . = ALIGN(0x100);
        stack_start = .;
        KEEP(*(.data.stack))
        . = stack_start + 0x400;
        KEEP(*(.data.after))
        KEEP(*(.data .data.*))
    }

    __global_pointer$ = . + 0x800;

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr)
    }
}

ASSERT(ADDR(.text) == 0x1000, "text VMA");
ASSERT(ADDR(.data) == 0x2000, "data VMA");
