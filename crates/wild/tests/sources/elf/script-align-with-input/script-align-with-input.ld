MEMORY {
    ram (rwx) : ORIGIN = 0x10000, LENGTH = 0x10000
    rom (rx) : ORIGIN = 0x20004, LENGTH = 0x10000
}

SECTIONS {
    .text : {
        KEEP(*(.text .text.*))
    } > ram AT> rom

    /* 1-byte, 1-aligned: leaves the next VMA/LMA one byte into the next 16. */
    .a : ALIGN_WITH_INPUT {
        KEEP(*(.a))
    } > ram AT> rom

    /* 16-aligned. ALIGN_WITH_INPUT adds the VMA pad to LMA so the delta matches .a. */
    .b : ALIGN_WITH_INPUT {
        KEEP(*(.b))
    } > ram AT> rom
}

ASSERT(LOADADDR(.a) - ADDR(.a) == LOADADDR(.b) - ADDR(.b), "ALIGN_WITH_INPUT VMA-LMA delta");
