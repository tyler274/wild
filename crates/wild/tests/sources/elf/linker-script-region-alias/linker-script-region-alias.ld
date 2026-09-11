MEMORY {
    RAM : ORIGIN = 0x10000000, LENGTH = 0x100000
    ROM : ORIGIN = 0x20000000, LENGTH = 0x100000
}

REGION_ALIAS("ALIAS_RAM", RAM)
REGION_ALIAS("ALIAS_ROM", ROM)

SECTIONS {
    .data.ram1 : {
        KEEP(*(.data.ram1))
    } > ALIAS_RAM

    .data.ram2 : {
        KEEP(*(.data.ram2))
    } > ALIAS_RAM

    .text : {
        *(.text)
    } > ALIAS_ROM
}

ASSERT(ORIGIN(ALIAS_RAM) == 0x10000000, "ORIGIN(ALIAS_RAM) is incorrect")
ASSERT(LENGTH(ALIAS_RAM) == 0x100000, "LENGTH(ALIAS_RAM) is incorrect")
ASSERT(ORIGIN(ALIAS_ROM) == 0x20000000, "ORIGIN(ALIAS_ROM) is incorrect")
ASSERT(ADDR(.data.ram1) == 0x10000000, "data.ram1 address is wrong")
ASSERT(ADDR(.data.ram2) == 0x10000004, "data.ram2 address is wrong")
