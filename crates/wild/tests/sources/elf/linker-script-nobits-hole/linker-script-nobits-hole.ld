ENTRY(_start)

SECTIONS {
    . = 0x400000;
    .text : {
        KEEP(*(.text .text.*))
    }
    .bss : {
        KEEP(*(.bss .bss.*))
    }
    .after_bss : {
        KEEP(*(.after_bss))
    }
}
