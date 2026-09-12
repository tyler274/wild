ENTRY(_start)
LD_FEATURE("SANE_EXPR")

e1 = 0x100;
e2 = 0x80;
e3 = e1 * e2;
ASSERT(e3 == 0x8000, "e3 must be 0x8000");

SECTIONS {
    . = e3;
    .text : { *(.text .text.*) }
    .data : { *(.data .data.*) }
}
