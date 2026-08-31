ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
}

SECTIONS {
    . = 0x400004;
    .text : {
        _stext = .;
        . = ALIGN(16);
        aligned_sym = .;
        KEEP(*(.text .text.*))
    } :text
    _etext = .;

    . = ALIGN(0x1000);
    .init.begin : {
        __init_begin = .;
    }

    text_size = _etext - _stext;
    .orc_lookup : {
        orc_lookup = .;
        . += (((text_size + 256 - 1) / 256) + 1) * 4;
        orc_lookup_end = .;
    }

    .unused_zero 0 : {
        unused_zero = .;
    }

    __global_pointer$ = . + 0x800;

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .data .data.*)
    }
}

ASSERT(_stext == 0x400004, "_stext is the script section start");
ASSERT(ADDR(.text) == 0x400004, "headers outside LOAD must not consume VMA");
ASSERT(aligned_sym == 0x400010, "ALIGN(n) aligns the absolute VMA");
ASSERT(__init_begin == ALIGN(ADDR(.text) + SIZEOF(.text), 0x1000), "empty section inherits the location counter");
ASSERT(ADDR(.init.begin) == __init_begin, "empty section ADDR is the location counter");
ASSERT(unused_zero == 0, "explicit address 0 stays at 0");
ASSERT(_etext == ADDR(.text) + SIZEOF(.text), "_etext is the end of .text");
ASSERT(text_size == SIZEOF(.text), "text_size is _etext - _stext");
ASSERT(orc_lookup_end - orc_lookup == (((text_size + 256 - 1) / 256) + 1) * 4, "orc_lookup reservation uses text_size");
ASSERT(ADDR(.orc_lookup) == __init_begin, "reserved section follows the empty marker");
