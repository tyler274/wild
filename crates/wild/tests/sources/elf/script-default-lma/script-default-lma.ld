MEMORY {
    ram (rwx) : ORIGIN = 0x10000, LENGTH = 0x10000
    rom (rx) : ORIGIN = 0x20000, LENGTH = 0x10000
}

SECTIONS {
    .text : {
        KEEP(*(.text .text.*))
    } > ram AT> rom

    .a : {
        KEEP(*(.a))
    } > ram

    .b : {
        KEEP(*(.b))
    } > ram
}

/* GNU: later sections in the same VMA region keep the running LMA cursor
   from the previous AT> (not LMA = VMA, and not a constant VMA-LMA delta). */
ASSERT(LOADADDR(.text) != ADDR(.text), ".text LMA differs from VMA");
ASSERT(LOADADDR(.a) != ADDR(.a), ".a keeps a distinct LMA");
ASSERT(LOADADDR(.a) >= LOADADDR(.text), ".a LMA continues after .text");
ASSERT(LOADADDR(.b) != ADDR(.b), ".b keeps a distinct LMA");
ASSERT(LOADADDR(.b) >= LOADADDR(.a), ".b LMA continues after .a");
