MEMORY {
    ROM (rx) : ORIGIN = 0x10000000, LENGTH = 0x100000
    RAM (rw) : ORIGIN = 0x20000000, LENGTH = 0x100000
}

SECTIONS {
    .text : {
        KEEP(*(.text .text.*))
    } > ROM AT> RAM

    .data : {
        KEEP(*(.data .data.*))
    } > RAM
}

ASSERT(ADDR(.text) == 0x10000000, "VMA of .text");
ASSERT(LOADADDR(.text) == 0x20000000, "LMA of .text via AT>RAM");
ASSERT(ADDR(.data) >= 0x20000000, "VMA of .data");
