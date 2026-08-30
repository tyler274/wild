ENTRY(_start)

SECTIONS {
    . = 0x401000;
    .text.keep : {
        *(EXCLUDE_FILE(*exclude-file-drop*.o) .text.special)
    }
    .text : {
        *(.text .text.*)
    }
    .eh_frame : { *(.eh_frame) }
}
