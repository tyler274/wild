ENTRY(_start)

SECTIONS {
    . = 0x400000;
    .text : { KEEP(*(.text*)) }
    .data : { KEEP(*(.data*)) }
    /* Unaligned VMA so ALIGN(ALIGNOF(.bss)) is observable. Empty .data1 must
       be skipped, matching GNU ld's "next allocated section". */
    . = 0x401001;
    . = ALIGN(ALIGNOF(NEXT_SECTION));
    __bss_start = .;
    .data1 : { *(.data1) }
    .bss : {
        KEEP(*(.bss*))
        KEEP(*(COMMON))
    }
    /DISCARD/ : { *(.comment .note .note.* .eh_frame .eh_frame_hdr) }
}

ASSERT(__bss_start == 0x401020, "ALIGNOF(NEXT_SECTION) should pad to .bss alignment");
