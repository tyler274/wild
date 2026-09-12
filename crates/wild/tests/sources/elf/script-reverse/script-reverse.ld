ENTRY(_start)

SECTIONS {
    . = 0x401000;
    .text : {
        *(.text)
        /* Input order is aaa, ccc, bbb. Reverse-by-name is ccc, bbb, aaa. */
        *(REVERSE(.text.rev.*))
        *(SORT_BY_INIT_PRIORITY(REVERSE(.init_array.*)))
    }
    .eh_frame : { *(.eh_frame) }
}

ASSERT(func_ccc < func_bbb, "REVERSE name order is wrong");
ASSERT(func_bbb < func_aaa, "REVERSE name order is wrong");
ASSERT(prio_300 < prio_200, "REVERSE init priority order is wrong");
ASSERT(prio_200 < prio_100, "REVERSE init priority order is wrong");
