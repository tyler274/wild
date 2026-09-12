ENTRY(_start)

SECTIONS {
    . = 0x401000;
    .text : {
        *(.text)
        *(SORT_BY_ALIGNMENT(REVERSE(.text.align.*)))
    }
    .eh_frame : { *(.eh_frame) }
}

ASSERT(align_64 < align_32, "SORT_BY_ALIGNMENT(REVERSE) should stay descending");
ASSERT(align_32 < align_16, "SORT_BY_ALIGNMENT(REVERSE) should stay descending");
ASSERT(align_16 < align_4, "SORT_BY_ALIGNMENT(REVERSE) should stay descending");
