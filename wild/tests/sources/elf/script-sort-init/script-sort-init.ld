ENTRY(_start)

SECTIONS {
    . = 0x401000;
    .text : {
        *(.text)
        *(SORT_BY_INIT_PRIORITY(.init_array.*))
        *(.init_array)
    }
    .eh_frame : { *(.eh_frame) }
}

ASSERT(prio_100 < prio_200, "init priority order is wrong");
ASSERT(prio_200 < prio_65535, "default init_array should be last");
