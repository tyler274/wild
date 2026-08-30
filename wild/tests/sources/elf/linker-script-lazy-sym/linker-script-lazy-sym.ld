LOAD_OFFSET = 0x1000;

SECTIONS {
    .text 0x2000 : AT(ADDR(.text) - LOAD_OFFSET) {
        *(.text .text.*)
        __global_pointer$ = . + 0x800;
    }
}

ASSERT(ADDR(.text) == 0x2000, "VMA");
ASSERT(LOADADDR(.text) == 0x1000, "LMA uses LOAD_OFFSET");
