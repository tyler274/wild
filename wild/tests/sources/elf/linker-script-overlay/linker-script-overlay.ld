SECTIONS {
    OVERLAY 0x1000 : AT(0x2000) {
        .ovl1 { KEEP(*(.ovl1)) }
        .ovl2 { KEEP(*(.ovl2)) }
    }
    .text : { *(.text .text.*) }
}

ASSERT(ADDR(.ovl1) == ADDR(.ovl2), "overlay members share VMA");
ASSERT(LOADADDR(.ovl1) == 0x2000, "first overlay LMA");
