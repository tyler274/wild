MEMORY {
    ROM (rx) : ORIGIN = 0x10000000, LENGTH = 0x100000
    RAM (rw) : ORIGIN = 0x20000000, LENGTH = 0x100000
}

SECTIONS {
    .text : { KEEP(*(.text .text.*)) }
    .data : { KEEP(*(.data .data.*)) }
}

ASSERT(ADDR(.text) == 0x10000000, "text auto-picked ROM");
ASSERT(ADDR(.data) == 0x20000000, "data auto-picked RAM");
