MEMORY {
    ram (rwx) : ORIGIN = 0x10000, LENGTH = 0x10000
    rom (rx) : ORIGIN = 0x20000, LENGTH = 0x10000
}

SECTIONS {
    .text : {
        KEEP(*(.text .text.*))
    } > ram AT> rom

    .a 0x18000 : {
        KEEP(*(.a))
    }
}

ASSERT(LOADADDR(.a) == ADDR(.a), "specific VMA sets LMA = VMA");
