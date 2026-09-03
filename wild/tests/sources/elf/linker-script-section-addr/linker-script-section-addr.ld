ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = 0x400004;
    .text ALIGN(0) : {
        _stext = .;
        KEEP(*(.text .text.*))
        _etext = .;
    } :text

    .data ALIGN(0x1000) : {
        data_start = .;
        KEEP(*(.data .data.*))
    } :data

    .bss ((LOADADDR(.data) + SIZEOF(.data) + 0xf) & ~ 0xf) : {
        bss_start = .;
        KEEP(*(.bss .bss.*))
        *(COMMON)
    } :data

    __global_pointer$ = . + 0x800;

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .rodata*)
    }
}

ASSERT(_stext == 0x400004, "ALIGN(0) as a section address is a no-op");
ASSERT(ADDR(.text) == 0x400004, ".text stays at the location counter");
ASSERT(data_start == ALIGN(_etext, 0x1000), ".data ALIGN(0x1000) aligns the current VMA");
ASSERT(ADDR(.data) == data_start, "ADDR(.data) matches the ALIGN address");
ASSERT(bss_start == ((LOADADDR(.data) + SIZEOF(.data) + 0xf) & ~ 0xf), ".bss address uses LOADADDR and SIZEOF");
ASSERT(ADDR(.bss) == bss_start, "ADDR(.bss) matches the computed address");
