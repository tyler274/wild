SECTIONS {
    .text : AT(0x20000) {
        KEEP(*(.text .text.*))
    }

    .a : {
        KEEP(*(.a))
    }

    .b : {
        KEEP(*(.b))
    }
}

/* Without MEMORY, GNU does not continue LMA after an explicit AT(); LMA = VMA. */
ASSERT(LOADADDR(.text) == 0x20000, ".text LMA from AT()");
ASSERT(LOADADDR(.a) == ADDR(.a), ".a LMA equals VMA without MEMORY");
ASSERT(LOADADDR(.b) == ADDR(.b), ".b LMA equals VMA without MEMORY");
