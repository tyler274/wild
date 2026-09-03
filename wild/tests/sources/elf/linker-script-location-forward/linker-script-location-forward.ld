ENTRY(_start)

PHDRS {
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS {
    . = later_sum;
    .text : {
        _stext = .;
        KEEP(*(.text .text.*))
    } :text

    .data : {
        KEEP(*(.data .data.*))
    } :data

    __global_pointer$ = . + 0x800;

    /DISCARD/ : {
        *(.comment .note .note.* .eh_frame .eh_frame_hdr .rodata*)
    }
}

BASE = 0x400000;
OFFSET = 0x1000;
later_sum = BASE + OFFSET;

ASSERT(ADDR(.text) == 0x401000, "forward constant chain sets the location counter");
ASSERT(_stext == 0x401000, "_stext follows later_sum");
