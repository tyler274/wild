SECTIONS {
    .rodata : {
        *(.rodata .rodata.*)
        BYTE(0)
        SHORT(0x2233)
        LONG(0x44556677)
        QUAD(0x8899aabbccddeeff)
    }
    .text : { *(.text .text.*) }
}
